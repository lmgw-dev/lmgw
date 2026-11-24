//! This machine's own addresses for web sources (document-conversion design
//! ruling 9, §7.4.1): an address outside the fixed refused ranges
//! ([`super::ranges`]) can still be this computer or its network, for
//! example a global IPv6 address on a home LAN (every device behind the
//! router's inbound firewall has one) or a server's own public address.
//! Such an address is refused like a private one, unless the allowed sites
//! name that address with the hop's port.
//!
//! What counts as local:
//! - every address of this machine's interfaces, and every address inside
//!   one of their on-link prefixes (`192.0.2.20/24` makes `192.0.2.0/24`
//!   local), read with `if_addrs` (`getifaddrs` on Unix,
//!   `GetAdaptersAddresses` on Windows);
//! - the addresses the gateway's own listeners are bound to (the plain
//!   listener's `bind_addr`, the TLS listener's `tls.listen`), also when the
//!   interface list does not show them.
//!
//! An interface whose prefix is `/0` counts with its address alone: a
//! prefix that long would make the whole internet this computer's network.
//! When the interfaces cannot be read, the fixed ranges and the listeners
//! apply alone and the log says so once.

use std::net::IpAddr;
use std::sync::Once;

/// This machine's addresses and on-link prefixes, read once per request.
#[derive(Debug, Clone, Default)]
pub struct Local {
    /// Interface addresses with their prefix length.
    nets: Vec<(IpAddr, u8)>,
    /// The gateway's own listeners' addresses.
    listeners: Vec<IpAddr>,
}

impl Local {
    /// Nothing local (tests that check the fixed ranges alone).
    pub fn none() -> Self {
        Self::default()
    }

    /// From an interface list and the listeners' addresses.
    pub fn from_parts(nets: Vec<(IpAddr, u8)>, listeners: Vec<IpAddr>) -> Self {
        Self {
            nets: nets
                .into_iter()
                .map(|(ip, len)| (ip.to_canonical(), len))
                .collect(),
            listeners: listeners
                .into_iter()
                .map(|ip| ip.to_canonical())
                .filter(|ip| !ip.is_unspecified())
                .collect(),
        }
    }

    /// This machine now, with `listeners`.
    pub fn read(listeners: Vec<IpAddr>) -> Self {
        let nets = match if_addrs::get_if_addrs() {
            Ok(ifaces) => ifaces
                .into_iter()
                .map(|i| match i.addr {
                    if_addrs::IfAddr::V4(a) => (IpAddr::V4(a.ip), a.prefixlen),
                    if_addrs::IfAddr::V6(a) => (IpAddr::V6(a.ip), a.prefixlen),
                })
                .collect(),
            Err(e) => {
                static SAID: Once = Once::new();
                SAID.call_once(|| {
                    tracing::warn!(
                        "web sources: this computer's network interfaces could not be read \
                         ({e}); its own addresses outside the fixed private ranges are not \
                         refused until they can be"
                    );
                });
                Vec::new()
            }
        };
        Self::from_parts(nets, listeners)
    }

    /// The gateway's own listeners' addresses, from the settings in force.
    pub fn listeners(s: &crate::config::Settings) -> Vec<IpAddr> {
        let mut out = Vec::new();
        if let Ok(a) = s.bind_addr.parse::<std::net::SocketAddr>() {
            out.push(a.ip());
        }
        if let Some(t) = s.tls.listener() {
            out.push(t.addr.ip());
        }
        out
    }

    /// Why `ip` is this machine's, in words; `None` when it is not.
    pub fn class(&self, ip: IpAddr) -> Option<&'static str> {
        let ip = ip.to_canonical();
        if self.listeners.contains(&ip) {
            return Some("this gateway's own listener address");
        }
        for (net, _) in &self.nets {
            if *net == ip {
                return Some("this computer's own address (a network interface)");
            }
        }
        for (net, len) in &self.nets {
            if *len > 0 && same_prefix(*net, ip, *len) {
                return Some("this computer's local network (an interface's on-link prefix)");
            }
        }
        None
    }
}

/// Whether `a` and `b` share their first `len` bits (one family).
fn same_prefix(a: IpAddr, b: IpAddr, len: u8) -> bool {
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let len = u32::from(len.min(32));
            let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let len = u32::from(len.min(128));
            let mask = if len == 0 {
                0
            } else {
                u128::MAX << (128 - len)
            };
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn interface_addresses_their_prefixes_and_listeners_are_local() {
        let l = Local::from_parts(
            vec![
                (ip("2001:db8:20:1::7"), 64),
                (ip("203.0.113.20"), 29),
                (ip("198.51.100.9"), 0),
            ],
            vec![ip("192.0.2.80"), ip("0.0.0.0")],
        );
        assert!(l
            .class(ip("2001:db8:20:1::7"))
            .unwrap()
            .contains("own address"));
        assert!(l
            .class(ip("2001:db8:20:1::99"))
            .unwrap()
            .contains("local network"));
        assert!(l.class(ip("2001:db8:20:2::99")).is_none());
        assert!(l.class(ip("203.0.113.23")).is_some());
        assert!(l.class(ip("203.0.113.24")).is_none());
        // A mapped address is its IPv4 address.
        assert!(l.class(ip("::ffff:203.0.113.21")).is_some());
        // A /0 interface is its address alone.
        assert!(l.class(ip("198.51.100.9")).is_some());
        assert!(l.class(ip("198.51.100.10")).is_none());
        assert!(l.class(ip("192.0.2.80")).unwrap().contains("listener"));
        // An unspecified bind is no listener address.
        assert!(l.class(ip("0.0.0.0")).is_none());
    }
}
