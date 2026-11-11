//! Whether a request came straight from this machine's loopback (headless
//! server design D51): the test a loopback rule reads (D26's exemption for
//! a password over plain HTTP, WP6), never the client address alone.

use super::Conn;

impl Conn {
    /// A loopback client that reached lmgw without a proxy. A request that
    /// came through a trusted proxy (`proxy` set) crossed a network hop
    /// whatever client address the proxy forwarded, `127.0.0.1` included,
    /// so it is never this.
    pub fn is_direct_loopback(&self) -> bool {
        self.proxy.is_none() && self.peer.ip().is_loopback()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr(t: &str) -> SocketAddr {
        t.parse().unwrap()
    }

    #[test]
    fn a_proxied_request_is_never_direct_loopback() {
        let lo = addr("127.0.0.1:8787");
        assert!(Conn::plain(addr("127.0.0.1:40000"), lo).is_direct_loopback());
        assert!(Conn::plain(addr("[::1]:40000"), addr("[::1]:8787")).is_direct_loopback());
        assert!(
            Conn::plain(addr("[::ffff:127.0.0.1]:40000"), lo).is_direct_loopback(),
            "a mapped loopback peer is canonical"
        );
        assert!(!Conn::plain(addr("192.0.2.5:40000"), lo).is_direct_loopback());
        // A proxy on loopback that forwarded a loopback client, or named
        // none: it crossed a hop.
        for client in [
            addr("127.0.0.1:0"),
            crate::server::forwarded::UNKNOWN_CLIENT,
        ] {
            let proxied = Conn {
                peer: client,
                proxy: Some(addr("127.0.0.1:40000")),
                ..Conn::plain(addr("127.0.0.1:40000"), lo)
            };
            assert!(!proxied.is_direct_loopback(), "{client}");
        }
    }
}
