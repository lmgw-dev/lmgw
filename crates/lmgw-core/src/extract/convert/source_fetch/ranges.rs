//! The address ranges a web source's hop may not connect to
//! (document-conversion design ruling 9, §7.4.1): every address a host
//! resolves to is classed here before the connection, and the connection
//! goes to exactly the addresses classed.
//!
//! Two kinds of refusal:
//! - [`Class::UnlessListed`]: loopback, private (RFC 1918), link-local
//!   (169.254.0.0/16 with the cloud metadata address, fe80::/10), carrier-
//!   grade NAT (100.64.0.0/10), unique-local IPv6, documentation and
//!   benchmarking ranges, the IETF and transition ranges. Fetched only when
//!   the allowed list names that address itself ([`super::hosts`]).
//! - [`Class::Never`]: unspecified (0.0.0.0/8, `::`; Linux connects
//!   0.0.0.0 to loopback), multicast, broadcast and the reserved 240.0.0.0/4.
//!   Nothing to fetch there; refused even when listed.
//!
//! IPv6 is public only inside global unicast (2000::/3) and outside its
//! special blocks. An IPv4-mapped (`::ffff:a.b.c.d`) or NAT64
//! (`64:ff9b::a.b.c.d`) address is classed by its IPv4 address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// An address's class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Public,
    /// Refused unless the list names the address; the range in words.
    UnlessListed(&'static str),
    /// Refused even when listed; the range in words.
    Never(&'static str),
}

/// `ip`'s class.
pub fn class(ip: IpAddr) -> Class {
    match ip {
        IpAddr::V4(a) => class_v4(a),
        IpAddr::V6(a) => class_v6(a),
    }
}

fn in_v4(a: Ipv4Addr, net: [u8; 4], len: u32) -> bool {
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    (u32::from(a) & mask) == (u32::from(Ipv4Addr::from(net)) & mask)
}

/// The IPv4 ranges, most specific first where they nest.
const V4: &[([u8; 4], u32, Class)] = &[
    ([0, 0, 0, 0], 8, Class::Never("this network (0.0.0.0/8)")),
    (
        [10, 0, 0, 0],
        8,
        Class::UnlessListed("private (10.0.0.0/8)"),
    ),
    (
        [100, 64, 0, 0],
        10,
        Class::UnlessListed("carrier-grade NAT (100.64.0.0/10)"),
    ),
    (
        [127, 0, 0, 0],
        8,
        Class::UnlessListed("loopback (127.0.0.0/8)"),
    ),
    (
        [169, 254, 0, 0],
        16,
        Class::UnlessListed("link-local (169.254.0.0/16, with cloud metadata services)"),
    ),
    (
        [172, 16, 0, 0],
        12,
        Class::UnlessListed("private (172.16.0.0/12)"),
    ),
    (
        [192, 0, 0, 0],
        24,
        Class::UnlessListed("IETF protocol assignments (192.0.0.0/24)"),
    ),
    (
        [192, 0, 2, 0],
        24,
        Class::UnlessListed("documentation (192.0.2.0/24)"),
    ),
    (
        [192, 88, 99, 0],
        24,
        Class::UnlessListed("6to4 relay anycast (192.88.99.0/24)"),
    ),
    (
        [192, 168, 0, 0],
        16,
        Class::UnlessListed("private (192.168.0.0/16)"),
    ),
    (
        [198, 18, 0, 0],
        15,
        Class::UnlessListed("benchmarking (198.18.0.0/15)"),
    ),
    (
        [198, 51, 100, 0],
        24,
        Class::UnlessListed("documentation (198.51.100.0/24)"),
    ),
    (
        [203, 0, 113, 0],
        24,
        Class::UnlessListed("documentation (203.0.113.0/24)"),
    ),
    ([224, 0, 0, 0], 4, Class::Never("multicast (224.0.0.0/4)")),
    (
        [255, 255, 255, 255],
        32,
        Class::Never("broadcast (255.255.255.255)"),
    ),
    ([240, 0, 0, 0], 4, Class::Never("reserved (240.0.0.0/4)")),
];

fn class_v4(a: Ipv4Addr) -> Class {
    V4.iter()
        .find(|(net, len, _)| in_v4(a, *net, *len))
        .map_or(Class::Public, |(_, _, c)| *c)
}

fn in_v6(a: Ipv6Addr, net: [u16; 8], len: u32) -> bool {
    let mask = if len == 0 {
        0
    } else {
        u128::MAX << (128 - len)
    };
    (u128::from(a) & mask) == (u128::from(Ipv6Addr::from(net)) & mask)
}

/// The IPv6 ranges inside or beside 2000::/3 that are not public.
const V6: &[([u16; 8], u32, Class)] = &[
    (
        [0, 0, 0, 0, 0, 0, 0, 0],
        128,
        Class::Never("unspecified (::)"),
    ),
    (
        [0, 0, 0, 0, 0, 0, 0, 1],
        128,
        Class::UnlessListed("loopback (::1)"),
    ),
    (
        [0x64, 0xff9b, 1, 0, 0, 0, 0, 0],
        48,
        Class::UnlessListed("NAT64 local use (64:ff9b:1::/48)"),
    ),
    (
        [0x100, 0, 0, 0, 0, 0, 0, 0],
        64,
        Class::UnlessListed("discard-only (100::/64)"),
    ),
    (
        [0x2001, 0xdb8, 0, 0, 0, 0, 0, 0],
        32,
        Class::UnlessListed("documentation (2001:db8::/32)"),
    ),
    (
        [0x2001, 0, 0, 0, 0, 0, 0, 0],
        23,
        Class::UnlessListed("IETF protocol assignments with Teredo (2001::/23)"),
    ),
    (
        [0x2002, 0, 0, 0, 0, 0, 0, 0],
        16,
        Class::UnlessListed("6to4 (2002::/16)"),
    ),
    (
        [0x3fff, 0, 0, 0, 0, 0, 0, 0],
        20,
        Class::UnlessListed("documentation (3fff::/20)"),
    ),
    (
        [0x5f00, 0, 0, 0, 0, 0, 0, 0],
        16,
        Class::UnlessListed("segment routing (5f00::/16)"),
    ),
    (
        [0xfc00, 0, 0, 0, 0, 0, 0, 0],
        7,
        Class::UnlessListed("unique local (fc00::/7)"),
    ),
    (
        [0xfe80, 0, 0, 0, 0, 0, 0, 0],
        10,
        Class::UnlessListed("link-local (fe80::/10)"),
    ),
    (
        [0xfec0, 0, 0, 0, 0, 0, 0, 0],
        10,
        Class::UnlessListed("site-local (fec0::/10)"),
    ),
    (
        [0xff00, 0, 0, 0, 0, 0, 0, 0],
        8,
        Class::Never("multicast (ff00::/8)"),
    ),
];

fn class_v6(a: Ipv6Addr) -> Class {
    if let Some(v4) = a.to_ipv4_mapped() {
        return class_v4(v4);
    }
    // NAT64's well-known prefix carries an IPv4 address in its last 32 bits.
    if in_v6(a, [0x64, 0xff9b, 0, 0, 0, 0, 0, 0], 96) {
        let o = a.octets();
        return class_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    if let Some((_, _, c)) = V6.iter().find(|(net, len, _)| in_v6(a, *net, *len)) {
        return *c;
    }
    // IPv4-compatible (deprecated) and the rest of ::/8.
    if in_v6(a, [0, 0, 0, 0, 0, 0, 0, 0], 8) {
        return Class::UnlessListed("reserved (::/8, with IPv4-compatible addresses)");
    }
    if in_v6(a, [0x2000, 0, 0, 0, 0, 0, 0, 0], 3) {
        Class::Public
    } else {
        Class::UnlessListed("outside IPv6 global unicast (2000::/3)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Class {
        class(s.parse().unwrap())
    }

    #[test]
    fn public_addresses_pass() {
        for a in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.0",
            "172.32.0.1",
            "192.169.0.1",
            "2606:4700::1111",
            "64:ff9b::808:808",
            "::ffff:8.8.8.8",
        ] {
            assert_eq!(c(a), Class::Public, "{a}");
        }
    }

    #[test]
    fn every_refused_range_is_refused_with_its_name() {
        for (a, words) in [
            ("127.0.0.1", "loopback"),
            ("127.255.0.9", "loopback"),
            ("10.1.2.3", "private"),
            ("172.16.0.1", "private"),
            ("172.31.255.255", "private"),
            ("192.168.1.1", "private"),
            ("169.254.169.254", "link-local"),
            ("100.64.0.1", "carrier-grade"),
            ("100.127.255.255", "carrier-grade"),
            ("192.0.2.1", "documentation"),
            ("198.51.100.1", "documentation"),
            ("203.0.113.1", "documentation"),
            ("198.18.0.1", "benchmarking"),
            ("192.0.0.8", "IETF"),
            ("::1", "loopback"),
            ("fd00::1", "unique local"),
            ("fc00::1", "unique local"),
            ("fe80::1", "link-local"),
            ("fec0::1", "site-local"),
            ("2001:db8::1", "documentation"),
            ("3fff::1", "documentation"),
            ("2001::1", "Teredo"),
            ("2002:7f00:1::", "6to4"),
            ("64:ff9b:1::1", "NAT64"),
            ("::ffff:127.0.0.1", "loopback"),
            ("::ffff:172.16.0.2", "private"),
            ("::ffff:169.254.169.254", "link-local"),
            ("::ffff:100.64.0.1", "carrier-grade"),
            ("64:ff9b::7f00:1", "loopback"),
            ("64:ff9b::a9fe:a9fe", "link-local"),
            ("::127.0.0.1", "IPv4-compatible"),
            ("4000::1", "global unicast"),
        ] {
            match c(a) {
                Class::UnlessListed(w) => assert!(w.contains(words), "{a}: {w}"),
                other => panic!("{a}: {other:?}"),
            }
        }
    }

    #[test]
    fn unspecified_multicast_and_broadcast_are_never_fetched() {
        for (a, words) in [
            ("0.0.0.0", "this network"),
            ("0.1.2.3", "this network"),
            ("224.0.0.1", "multicast"),
            ("239.255.255.250", "multicast"),
            ("255.255.255.255", "broadcast"),
            ("240.0.0.1", "reserved"),
            ("::", "unspecified"),
            ("ff02::1", "multicast"),
            ("::ffff:0.0.0.0", "this network"),
            ("::ffff:224.0.0.1", "multicast"),
        ] {
            match c(a) {
                Class::Never(w) => assert!(w.contains(words), "{a}: {w}"),
                other => panic!("{a}: {other:?}"),
            }
        }
    }
}
