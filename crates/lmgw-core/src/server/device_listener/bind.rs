//! Where the device listener listens (device link design §4.1, D24), and
//! its sockets.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

use crate::config::{DevicesListen, Settings};

/// The connections a listening socket queues before its accept task takes
/// them: tokio's own default for `TcpListener::bind`, which the plain
/// listener has, so the device listener queues no fewer.
const BACKLOG: i32 = 1024;

/// Where the device listener serves: its scope and port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub listen: DevicesListen,
    pub port: u16,
}

impl Plan {
    /// The plan `settings` ask for: `None` while `devices.listener` is off;
    /// an error when its port cannot be derived (no `devices.port`, and an
    /// HTTP port with no number above it).
    pub fn of(settings: &Settings) -> Result<Option<Self>, String> {
        let d = &settings.devices;
        if !d.listener {
            return Ok(None);
        }
        let port = d.effective_port(&settings.bind_addr)?;
        Ok(Some(Self {
            listen: d.listen,
            port,
        }))
    }

    /// The addresses it binds: `127.0.0.1` and `[::1]` for `loopback`,
    /// `[::]` (every address, both families) for `all`, else the address.
    pub fn addresses(&self) -> Vec<SocketAddr> {
        match self.listen {
            DevicesListen::Loopback => vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), self.port),
            ],
            DevicesListen::All => {
                vec![SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    self.port,
                )]
            }
            DevicesListen::Addr(ip) => vec![SocketAddr::new(ip, self.port)],
        }
    }

    /// The settings a bind error names.
    pub fn settings_text(&self) -> String {
        format!(
            "devices.listen = {}, devices.port = {}",
            self.listen.as_text(),
            self.port
        )
    }
}

/// One listening socket on `addr`: address reuse (a move rebinds while the
/// old port's connections are in `TIME_WAIT`), dual-stack for `[::]`,
/// IPv6-only for any other IPv6 address, close-on-exec (socket2's default),
/// non-blocking for tokio.
fn listen_on(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    if let IpAddr::V6(ip) = addr.ip() {
        socket.set_only_v6(!ip.is_unspecified())?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(BACKLOG)?;
    TcpListener::from_std(socket.into())
}

/// Whether `e` says the machine has no IPv6 (the loopback pair's `[::1]`
/// then is skipped, and the IPv4 half serves alone).
fn no_ipv6(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EAFNOSUPPORT) | Some(libc::EADDRNOTAVAIL) | Some(libc::EPROTONOSUPPORT)
    )
}

/// Bind every address of `plan`; the error names the address, the reason
/// and the settings. A second address of the loopback pair takes the port
/// the first got (they differ only for a port of 0, which no setting holds).
pub(super) fn bind(plan: &Plan) -> Result<Vec<TcpListener>, String> {
    let mut sockets: Vec<TcpListener> = Vec::new();
    for addr in plan.addresses() {
        let addr = match sockets.first().and_then(|s| s.local_addr().ok()) {
            Some(first) => SocketAddr::new(addr.ip(), first.port()),
            None => addr,
        };
        match listen_on(addr) {
            Ok(s) => sockets.push(s),
            Err(e) if plan.listen == DevicesListen::Loopback && addr.is_ipv6() && no_ipv6(&e) => {
                tracing::warn!(
                    "the device listener serves 127.0.0.1 alone: binding [::1] failed ({e}), \
                     this machine has no IPv6 loopback"
                );
            }
            Err(e) => {
                return Err(format!(
                    "binding the device listener on {addr} failed: {e} ({})",
                    plan.settings_text()
                ))
            }
        }
    }
    Ok(sockets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_scope_binds_its_addresses() {
        let plan = |listen| Plan { listen, port: 8788 };
        assert_eq!(
            plan(DevicesListen::Loopback).addresses(),
            [
                "127.0.0.1:8788".parse::<SocketAddr>().unwrap(),
                "[::1]:8788".parse().unwrap()
            ]
        );
        assert_eq!(
            plan(DevicesListen::All).addresses(),
            ["[::]:8788".parse::<SocketAddr>().unwrap()]
        );
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(
            plan(DevicesListen::Addr(ip)).addresses(),
            ["192.0.2.1:8788".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn the_plan_follows_the_settings() {
        let mut s = Settings::default();
        assert_eq!(Plan::of(&s), Ok(None), "off by default");
        s.devices.listener = true;
        s.bind_addr = "127.0.0.1:8787".into();
        assert_eq!(
            Plan::of(&s),
            Ok(Some(Plan {
                listen: DevicesListen::Loopback,
                port: 8788
            }))
        );
        s.bind_addr = "127.0.0.1:65535".into();
        assert!(Plan::of(&s).unwrap_err().contains("65535"));
        s.devices.port = Some(9000);
        assert_eq!(Plan::of(&s).unwrap().unwrap().port, 9000);
    }

    #[tokio::test]
    async fn a_taken_port_is_an_error_naming_the_settings() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let plan = Plan {
            listen: DevicesListen::Addr("127.0.0.1".parse().unwrap()),
            port,
        };
        let e = bind(&plan).unwrap_err();
        assert!(e.contains(&format!("127.0.0.1:{port}")), "{e}");
        assert!(e.contains("devices.listen = 127.0.0.1"), "{e}");
        assert!(e.contains(&format!("devices.port = {port}")), "{e}");
    }
}
