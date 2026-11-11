//! The main listener's TLS and the public origin (headless server design §4,
//! D6–D9, §4.7): the `tls` section (`tls.cert_path`, `tls.key_path`,
//! `tls.listen`, `tls.handshake_timeout_s`) and the top-level `public_url`,
//! with their parsing.
//!
//! Listener settings of both deployments: ordinary dashboard settings on the
//! desktop, pinned from the environment in the server image (§2.1). The TLS
//! listener exists once the pair and `tls.listen` are set
//! ([`TlsSettings::listener`]); its address, the paths and the handshake
//! bound apply at the next start, like `bind_addr`, and a renewed pair at
//! the same paths is picked up by the next accept (D8).

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::net::Scheme;

/// The TLS listener's address the dashboard proposes on the desktop and the
/// server image takes when `LMGW_TLS_LISTEN` is not set (§2.1, §4.7).
pub const TLS_LISTEN_PROPOSED: &str = "0.0.0.0:8443";

/// `tls.handshake_timeout_s`'s default: the seconds a client gets from its
/// connection to the end of its TLS handshake, the device listener's
/// default for the same bound (`devices.handshake_timeout_s`).
pub const TLS_HANDSHAKE_TIMEOUT_S: u32 = 10;

/// The `tls` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct TlsSettings {
    /// The PEM certificate chain, the leaf first: an absolute path; empty =
    /// no TLS listener.
    pub cert_path: String,
    /// The PEM private key of the leaf: an absolute path; empty = no TLS
    /// listener.
    pub key_path: String,
    /// Where the TLS listener binds, `host:port`; empty = no TLS listener.
    pub listen: String,
    /// The seconds a client gets from its connection to the end of its TLS
    /// handshake; one that takes longer is closed. At least 1.
    pub handshake_timeout_s: u32,
}

impl Default for TlsSettings {
    fn default() -> Self {
        Self {
            cert_path: String::new(),
            key_path: String::new(),
            listen: String::new(),
            handshake_timeout_s: TLS_HANDSHAKE_TIMEOUT_S,
        }
    }
}

/// What a TLS listener is started with: the pair's paths and the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsListenerConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub addr: SocketAddr,
    pub handshake_timeout: std::time::Duration,
}

impl TlsSettings {
    /// `tls.listen` read, when it is an address and port.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.listen.trim().parse().ok()
    }

    /// Whether a pair is set: both paths.
    pub fn has_pair(&self) -> bool {
        !self.cert_path.trim().is_empty() && !self.key_path.trim().is_empty()
    }

    /// The TLS listener these settings describe: the pair and an address
    /// that parses. `None` without one of the three.
    pub fn listener(&self) -> Option<TlsListenerConfig> {
        if !self.has_pair() {
            return None;
        }
        Some(TlsListenerConfig {
            cert: PathBuf::from(self.cert_path.trim()),
            key: PathBuf::from(self.key_path.trim()),
            addr: self.listen_addr()?,
            handshake_timeout: std::time::Duration::from_secs(u64::from(
                self.handshake_timeout_s.max(1),
            )),
        })
    }

    /// Whether a TLS listener is configured.
    pub fn on(&self) -> bool {
        self.listener().is_some()
    }
}

/// A save's check of the `tls` section after its changes: each path empty or
/// absolute, both or neither, the address empty or `host:port`, the
/// handshake bound at least 1. Every refusal names the row on Settings.
pub fn validate_tls(t: &TlsSettings) -> Result<(), String> {
    use lmgw_api_types::settings_labels::{
        TLS_CERT_AT, TLS_HANDSHAKE_TIMEOUT_AT, TLS_KEY_AT, TLS_LISTEN_AT,
    };
    for (what, at, path) in [
        ("the certificate", TLS_CERT_AT, &t.cert_path),
        ("the private key", TLS_KEY_AT, &t.key_path),
    ] {
        let p = path.trim();
        // Absolute: `/…` on Unix; on Windows `C:\…` or `\\server\…`, never one
        // the working directory or its drive resolves (`tls.crt`, `C:tls.crt`,
        // `\tls.crt`, `/etc/tls.crt`).
        if !p.is_empty() && !std::path::Path::new(p).is_absolute() {
            return Err(format!(
                "{what}'s path ({at}) must be absolute, since the gateway's own process reads \
                 it; got '{p}'"
            ));
        }
    }
    let (cert, key) = (t.cert_path.trim().is_empty(), t.key_path.trim().is_empty());
    if cert != key {
        return Err(format!(
            "the certificate ({TLS_CERT_AT}) and its private key ({TLS_KEY_AT}) are set \
             together: a certificate without its key (or a key without its certificate) serves \
             nothing"
        ));
    }
    let listen = t.listen.trim();
    if !listen.is_empty() && listen.parse::<SocketAddr>().is_err() {
        return Err(format!(
            "the TLS address ({TLS_LISTEN_AT}) must be an address and port such as \
             {TLS_LISTEN_PROPOSED}; got '{listen}'"
        ));
    }
    if t.handshake_timeout_s == 0 {
        return Err(format!(
            "the handshake timeout ({TLS_HANDSHAKE_TIMEOUT_AT}) must be at least 1 second"
        ));
    }
    Ok(())
}

/// One TCP listener as [`tcp_port_collision`] sees it: what Settings calls
/// it, the variable that pins it in the server image, its addresses and its
/// port.
struct TcpListener {
    what: String,
    var: Option<&'static str>,
    addrs: Vec<std::net::IpAddr>,
    port: u16,
}

/// The TCP listeners `s` binds at a start: the plain one on `bind_addr`,
/// the TLS one while it is configured, the device listener while it is on.
fn tcp_listeners(s: &super::Settings) -> Vec<TcpListener> {
    use crate::config::deployment::Pinned;
    use lmgw_api_types::settings_labels::{BIND_ADDRESS_AT, DEVICE_LISTENER_AT, TLS_LISTEN_AT};
    let mut out = Vec::new();
    if let Ok(plain) = s.bind_addr.trim().parse::<SocketAddr>() {
        out.push(TcpListener {
            what: format!("the bind address {plain} ({BIND_ADDRESS_AT})"),
            var: Some(Pinned::BindAddr.variable()),
            addrs: vec![plain.ip()],
            port: plain.port(),
        });
    }
    if let Some(l) = s.tls.listener() {
        out.push(TcpListener {
            what: format!("the TLS address {} ({TLS_LISTEN_AT})", l.addr),
            var: Some(Pinned::TlsListen.variable()),
            addrs: vec![l.addr.ip()],
            port: l.addr.port(),
        });
    }
    if s.devices.listener {
        if let Ok(port) = s.devices.effective_port(&s.bind_addr) {
            out.push(TcpListener {
                what: format!(
                    "the device listener's port {port}{} ({DEVICE_LISTENER_AT})",
                    if s.devices.port.is_none() {
                        " (unset: the bind address's port plus one)"
                    } else {
                        ""
                    }
                ),
                var: None,
                addrs: super::webrtc::device_addrs(&s.devices.listen),
                port,
            });
        }
    }
    out
}

/// Whether two TCP listeners on `a` and `b` want one port on overlapping
/// addresses, so the second cannot bind: the same port (not 0, the OS's
/// pick) on equal addresses or under a wildcard of the other's family.
pub fn tcp_addrs_collide(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() != 0 && a.port() == b.port() && super::webrtc::addresses_overlap(a.ip(), b.ip())
}

/// The refusal of settings whose TCP listeners (the plain one, the TLS one,
/// the device listener) want one port on overlapping addresses: the second
/// bind would fail at the start (the server's start ends on it, a desktop's
/// TLS or device listener would never come up). `Ok` when a port is chosen
/// by the OS (0) or the addresses do not overlap. `container` names the
/// unit's variables beside the rows, since the image pins them from there.
/// The TCP twin of [`super::webrtc_port_collision`].
pub fn tcp_port_collision(s: &super::Settings, container: bool) -> Result<(), String> {
    let listeners = tcp_listeners(s);
    for (i, a) in listeners.iter().enumerate() {
        for b in &listeners[i + 1..] {
            if a.port == 0 || a.port != b.port {
                continue;
            }
            let overlap = a.addrs.iter().any(|x| {
                b.addrs
                    .iter()
                    .any(|y| super::webrtc::addresses_overlap(*x, *y))
            });
            if !overlap {
                continue;
            }
            let vars: Vec<&str> = [a.var, b.var].into_iter().flatten().collect();
            return Err(format!(
                "{} and {} are one TCP port on overlapping addresses: the second listener cannot \
                 bind it at the start. Change one of the two ports{}",
                a.what,
                b.what,
                if container && !vars.is_empty() {
                    format!(" (in the unit: {})", vars.join(", "))
                } else {
                    String::new()
                }
            ));
        }
    }
    Ok(())
}

/// `public_url` read (D9): the one origin every URL lmgw hands a person or
/// another machine is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicUrl {
    pub scheme: Scheme,
    /// The host as a URL spells it, lower-cased: a name, an IPv4 address or
    /// a bracketed IPv6 address.
    pub host: String,
    /// The port, the scheme's default when the URL names none.
    pub port: u16,
    /// Whether the URL names its port.
    pub explicit_port: bool,
}

impl PublicUrl {
    /// `https://lmgw.example:8443`: the origin without a trailing slash, the
    /// port only where the URL named one that is not the scheme's default.
    pub fn base(&self) -> String {
        format!("{}://{}", self.scheme.as_str(), self.authority())
    }

    /// `lmgw.example:8443`, or `lmgw.example` on the scheme's default port.
    pub fn authority(&self) -> String {
        if self.port == self.scheme.default_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The host without brackets: a name, or an IP address's text.
    pub fn bare_host(&self) -> &str {
        self.host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(&self.host)
    }
}

/// `public_url`'s text read: `http` or `https`, a host, an optional port, no
/// user, no path but `/`, no query and no fragment. Every refusal names the
/// setting and the value.
pub fn parse_public_url(text: &str) -> Result<PublicUrl, String> {
    let t = text.trim();
    let bad = |why: &str| {
        Err(format!(
            "the public URL ({}) must be an origin such as https://lmgw.example:8443 ({why}); \
             got '{t}'",
            lmgw_api_types::settings_labels::PUBLIC_URL_AT
        ))
    };
    let Ok(url) = reqwest::Url::parse(t) else {
        return bad("it does not parse as a URL");
    };
    let scheme = match url.scheme() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        _ => return bad("the scheme is http or https"),
    };
    if !url.username().is_empty() || url.password().is_some() {
        return bad("it carries no user or password");
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return bad("it has no path, query or fragment");
    }
    let Some(host) = url.host_str().filter(|h| !h.is_empty()) else {
        return bad("it names a host");
    };
    Ok(PublicUrl {
        scheme,
        host: host.to_ascii_lowercase(),
        port: url.port().unwrap_or(scheme.default_port()),
        explicit_port: url.port().is_some(),
    })
}

/// A save's check of `public_url`: empty, or [`parse_public_url`] passes.
pub fn validate_public_url(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Ok(());
    }
    parse_public_url(text).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `unix`, an absolute Unix path, as an absolute path of this system: on
    /// Windows one without a drive is not absolute, so it gets `C:`.
    fn abs(unix: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", unix.replace('/', "\\"))
        } else {
            unix.to_string()
        }
    }

    #[test]
    fn a_listener_needs_the_pair_and_an_address() {
        let mut t = TlsSettings::default();
        assert!(!t.on());
        t.cert_path = abs("/etc/lmgw/tls/tls.crt");
        t.key_path = abs("/etc/lmgw/tls/tls.key");
        assert!(!t.on(), "no address");
        t.listen = TLS_LISTEN_PROPOSED.into();
        let l = t.listener().unwrap();
        assert_eq!(l.addr, "0.0.0.0:8443".parse().unwrap());
        assert_eq!(l.handshake_timeout.as_secs(), 10);
        validate_tls(&t).unwrap();
    }

    #[test]
    fn a_save_refuses_half_a_pair_a_relative_path_and_a_bad_address() {
        let half = TlsSettings {
            cert_path: abs("/a.crt"),
            ..Default::default()
        };
        assert!(validate_tls(&half).unwrap_err().contains("set together"));
        let rel = TlsSettings {
            cert_path: "tls.crt".into(),
            key_path: abs("/k"),
            ..Default::default()
        };
        let e = validate_tls(&rel).unwrap_err();
        assert!(
            e.contains("the certificate's path (Settings → Network & access → TLS → Certificate) must be absolute"),
            "{e}"
        );
        let addr = TlsSettings {
            listen: "8443".into(),
            ..Default::default()
        };
        assert!(validate_tls(&addr)
            .unwrap_err()
            .contains("the TLS address (Settings → Network & access → TLS → TLS address)"));
        let zero = TlsSettings {
            handshake_timeout_s: 0,
            ..Default::default()
        };
        assert!(validate_tls(&zero)
            .unwrap_err()
            .contains("TLS → Handshake timeout) must be at least 1"));
    }

    /// Windows: a path with a drive and a root, or a UNC share, is absolute;
    /// one the working directory's drive resolves (`\…`, `/…`) or its
    /// directory on a drive (`C:tls.crt`) is refused.
    #[cfg(windows)]
    #[test]
    fn windows_paths_must_have_a_drive_and_a_root() {
        let with = |cert: &str| TlsSettings {
            cert_path: cert.into(),
            key_path: r"C:\lmgw\tls.key".into(),
            ..Default::default()
        };
        for ok in [
            r"C:\lmgw\tls.crt",
            "C:/lmgw/tls.crt",
            r"\\server\share\tls.crt",
        ] {
            validate_tls(&with(ok)).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for relative in [r"\certs\tls.crt", "/etc/tls.crt", "C:tls.crt", "tls.crt"] {
            let e = validate_tls(&with(relative)).unwrap_err();
            assert!(e.contains("must be absolute"), "{relative}: {e}");
        }
    }

    #[test]
    fn the_public_url_is_an_origin() {
        let u = parse_public_url("https://LMGW.example:8443/").unwrap();
        assert_eq!(u.base(), "https://lmgw.example:8443");
        assert_eq!((u.port, u.explicit_port), (8443, true));
        let u = parse_public_url("https://lmgw.example").unwrap();
        assert_eq!((u.base(), u.port), ("https://lmgw.example".into(), 443));
        let u = parse_public_url("https://lmgw.example:443").unwrap();
        assert_eq!(
            u.base(),
            "https://lmgw.example",
            "the default port is not spelled"
        );
        let u = parse_public_url("http://[2001:db8::5]:8787").unwrap();
        assert_eq!(u.base(), "http://[2001:db8::5]:8787");
        assert_eq!(u.bare_host(), "2001:db8::5");
        for bad in [
            "lmgw.example",
            "ftp://lmgw.example",
            "https://u:p@lmgw.example",
            "https://lmgw.example/ui",
            "https://lmgw.example/?a=1",
            "https://lmgw.example/#x",
        ] {
            let e = parse_public_url(bad).unwrap_err();
            assert!(
                e.starts_with("the public URL (Settings → Network & access → TLS → Public URL) must be an origin"),
                "{bad}: {e}"
            );
        }
        validate_public_url("").unwrap();
    }

    fn listeners(
        bind: &str,
        tls: &str,
        devices: Option<(&str, Option<u16>)>,
    ) -> super::super::Settings {
        let mut s = super::super::Settings {
            bind_addr: bind.into(),
            ..Default::default()
        };
        if !tls.is_empty() {
            s.tls.cert_path = "/c".into();
            s.tls.key_path = "/k".into();
            s.tls.listen = tls.into();
        }
        match devices {
            Some((listen, port)) => {
                s.devices.listener = true;
                s.devices.listen = super::super::DevicesListen::parse(listen).unwrap();
                s.devices.port = port;
            }
            None => s.devices.listener = false,
        }
        s
    }

    #[test]
    fn two_listeners_on_one_tcp_port_are_refused() {
        // Apart: fine.
        tcp_port_collision(&listeners("127.0.0.1:8787", "0.0.0.0:8443", None), false).unwrap();
        // The TLS address on the bind address's port, overlapping.
        let e = tcp_port_collision(&listeners("127.0.0.1:8443", "0.0.0.0:8443", None), false)
            .unwrap_err();
        assert!(
            e.contains("the bind address 127.0.0.1:8443 (Settings → Network & access → Gateway → Bind address)")
                && e.contains("the TLS address 0.0.0.0:8443 (Settings → Network & access → TLS → TLS address)")
                && e.contains("one TCP port"),
            "{e}"
        );
        assert!(!e.contains("LMGW_"), "the desktop has no unit: {e}");
        let e = tcp_port_collision(&listeners("127.0.0.1:8443", "0.0.0.0:8443", None), true)
            .unwrap_err();
        assert!(
            e.contains("(in the unit: LMGW_BIND_ADDR, LMGW_TLS_LISTEN)"),
            "{e}"
        );
        // Two concrete addresses that differ do not overlap.
        tcp_port_collision(&listeners("127.0.0.1:8443", "192.0.2.10:8443", None), false).unwrap();
        // The device listener's default port (the bind port plus one).
        let e = tcp_port_collision(
            &listeners("127.0.0.1:8787", "0.0.0.0:8788", Some(("loopback", None))),
            false,
        )
        .unwrap_err();
        assert!(
            e.contains("the device listener's port 8788 (unset: the bind address's port plus one)")
                && e.contains("Devices → Device listener"),
            "{e}"
        );
        tcp_port_collision(
            &listeners(
                "127.0.0.1:8787",
                "0.0.0.0:8788",
                Some(("loopback", Some(8790))),
            ),
            false,
        )
        .unwrap();
        // Chosen by the OS: never one port.
        tcp_port_collision(&listeners("127.0.0.1:0", "127.0.0.1:0", None), false).unwrap();
    }
}
