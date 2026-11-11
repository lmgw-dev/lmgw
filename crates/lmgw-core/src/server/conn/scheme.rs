//! The scheme a request came in with (headless server design §4.3, §4.5):
//! the listener's, or the one a trusted proxy said the client used, read
//! from its connect info.

use super::{Conn, Via};

impl Via {
    /// The scheme of the listener itself: `https` on the TLS and the device
    /// listener, `http` on the plain one. A request's own scheme is
    /// [`Conn::scheme`], which a trusted proxy can change; nothing else a
    /// client sends changes either.
    pub fn scheme(self) -> crate::net::Scheme {
        match self {
            Self::Plain => crate::net::Scheme::Http,
            Self::Tls | Self::Devices => crate::net::Scheme::Https,
        }
    }
}

impl Conn {
    /// The scheme the request came in with: what a trusted proxy said the
    /// client used (§4.5), else the listener's.
    pub fn scheme(&self) -> crate::net::Scheme {
        self.forwarded_scheme.unwrap_or(self.via.scheme())
    }
}

/// The scheme a request with these extensions came in with
/// ([`Conn::scheme`] of its `ConnectInfo<Conn>`); `http` for a router
/// served without connect info.
pub fn scheme_of(ext: &axum::http::Extensions) -> crate::net::Scheme {
    ext.get::<axum::extract::ConnectInfo<Conn>>()
        .map_or(crate::net::Scheme::Http, |ci| ci.0.scheme())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    #[test]
    fn the_scheme_is_the_listener_s() {
        use crate::net::Scheme;
        let mut ext = axum::http::Extensions::new();
        assert_eq!(scheme_of(&ext), Scheme::Http, "no connect info: plain");
        let lo: SocketAddr = "127.0.0.1:1".parse().unwrap();
        for (via, scheme) in [
            (Via::Plain, Scheme::Http),
            (Via::Tls, Scheme::Https),
            (Via::Devices, Scheme::Https),
        ] {
            ext.insert(axum::extract::ConnectInfo(Conn {
                via,
                ..Conn::plain(lo, lo)
            }));
            assert_eq!(scheme_of(&ext), scheme, "{via}");
        }
        // A trusted proxy's word wins over the listener's.
        ext.insert(axum::extract::ConnectInfo(Conn {
            forwarded_scheme: Some(Scheme::Https),
            ..Conn::plain(lo, lo)
        }));
        assert_eq!(scheme_of(&ext), Scheme::Https);
    }
}
