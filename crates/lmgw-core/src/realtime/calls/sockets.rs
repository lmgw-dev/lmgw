//! The calls' UDP sockets (calls design §3.1, §3.3): one per listener
//! address, each read and written through quinn-udp so that every datagram
//! carries the address it was sent to, and every reply leaves from the
//! address the client sent to.
//!
//! **Why the destination matters.** str0m's ICE agent drops a STUN request
//! whose destination is not one of the call's host candidates, and a socket
//! bound to `0.0.0.0` or `[::]` cannot tell by itself which of the host's
//! addresses a datagram was sent to. quinn-udp turns on `IP_PKTINFO` and
//! `IPV6_RECVPKTINFO` and hands each datagram's destination over
//! (`RecvMeta::dst_ip`); a reply's source goes the other way
//! (`Transmit::src_ip`), so it leaves from the candidate's address even on
//! a host with several. A `[::]` socket is dual-stack as the HTTP listener
//! on `[::]` is (the system's `bindv6only`), and reports IPv4 traffic as
//! `::ffff:a.b.c.d`: addresses are made canonical on the way in
//! ([`canonical`]) and mapped back on the way out ([`UdpSock::to_family`]).

use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};

use quinn_udp::{RecvMeta, Transmit, UdpSocketState};
use tokio::io::Interest;

/// The receive buffer: the largest a UDP datagram can be, which is also the
/// largest batch of datagrams the kernel's receive offload (GRO) coalesces
/// into one read, so nothing a socket delivers is ever cut. One per driver.
pub(super) const RECV_BUF: usize = u16::MAX as usize;

/// A socket's identity in the driver, never reused: a call names its socket
/// by it, so a socket that closed is never confused with a later one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct SockId(pub u64);

/// One bound UDP socket of a group.
pub(super) struct UdpSock {
    pub id: SockId,
    /// Its bound address: a wildcard IP for a wildcard bind.
    pub local: SocketAddr,
    socket: tokio::net::UdpSocket,
    state: UdpSocketState,
}

/// How a socket on an IPv6 address treats IPv4 (`IPV6_V6ONLY`), as the TCP
/// listener of its group does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum V6 {
    /// As the system's `bindv6only` has it: tokio's own bind, as the plain
    /// HTTP listener's.
    System,
    /// `[::]` dual-stack and any other IPv6 address IPv6-only, set on the
    /// socket: the device listener's own rule.
    DualOnWildcard,
}

/// Bind one socket on `addr` (port 0: chosen by the OS), its IPv6 stack as
/// `v6` says. Never with address reuse: on UDP that would let a second
/// socket share the port.
pub(super) async fn bind(id: SockId, addr: SocketAddr, v6: V6) -> io::Result<UdpSock> {
    let socket = match (v6, addr.ip()) {
        (V6::DualOnWildcard, IpAddr::V6(ip)) => {
            let s = socket2::Socket::new(
                socket2::Domain::IPV6,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            s.set_only_v6(!ip.is_unspecified())?;
            s.set_nonblocking(true)?;
            s.bind(&addr.into())?;
            tokio::net::UdpSocket::from_std(s.into())?
        }
        _ => tokio::net::UdpSocket::bind(addr).await?,
    };
    let state = UdpSocketState::new((&socket).into())?;
    let local = socket.local_addr()?;
    Ok(UdpSock {
        id,
        local,
        socket,
        state,
    })
}

/// One datagram as it came off a socket.
#[derive(Debug, Clone, Copy)]
pub(super) struct Datagram {
    /// Its byte range in the receive buffer.
    pub start: usize,
    pub end: usize,
    /// Where it came from, canonical.
    pub source: SocketAddr,
    /// Where it was sent to, canonical, on the socket's port.
    pub destination: SocketAddr,
}

/// Whether a socket bound on `local` serves a call whose candidate is `ip`:
/// its own address, or a wildcard of a family that reaches `ip` (`[::]`
/// reaches both).
pub(super) fn serves(local: SocketAddr, ip: IpAddr) -> bool {
    let own = local.ip();
    if !own.is_unspecified() {
        return own == ip;
    }
    own.is_ipv6() || ip.is_ipv4()
}

impl UdpSock {
    /// Poll for a datagram to read (the driver's select over every socket).
    pub fn poll_ready(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        self.socket.poll_recv_ready(cx).map(|_| ())
    }

    /// Read what one receive delivers into `buf`: one datagram, or several
    /// of one stride when the kernel coalesced them (GRO), split here.
    /// `Ok(None)`: nothing to read now (tokio's readiness is cleared).
    pub fn recv(&self, buf: &mut [u8], out: &mut Vec<Datagram>) -> io::Result<Option<()>> {
        let mut meta = [RecvMeta::default()];
        let read = self.socket.try_io(Interest::READABLE, || {
            self.state.recv(
                (&self.socket).into(),
                &mut [IoSliceMut::new(buf)],
                &mut meta,
            )
        });
        match read {
            Ok(n) => {
                for m in &meta[..n] {
                    self.split(m, out);
                }
                Ok(Some(()))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn split(&self, m: &RecvMeta, out: &mut Vec<Datagram>) {
        let source = canonical(m.addr);
        let dst_ip = m
            .dst_ip
            .map(|ip| ip.to_canonical())
            .unwrap_or_else(|| self.local.ip());
        let destination = SocketAddr::new(dst_ip, self.local.port());
        let stride = m.stride.max(1);
        let mut start = 0;
        while start < m.len {
            let end = (start + stride).min(m.len);
            out.push(Datagram {
                start,
                end,
                source,
                destination,
            });
            start = end;
        }
    }

    /// Send `contents` to `destination`, leaving from `source` (the call's
    /// candidate). A socket that would block drops it: UDP is lossy, and
    /// str0m retransmits what needs it. `false` when it was not sent.
    pub fn send(&self, source: SocketAddr, destination: SocketAddr, contents: &[u8]) -> bool {
        // A concrete bind leaves from its own address by itself.
        let src_ip = self
            .local
            .ip()
            .is_unspecified()
            .then(|| self.to_family(source).ip());
        let t = Transmit {
            destination: self.to_family(destination),
            ecn: None,
            contents,
            segment_size: None,
            src_ip,
        };
        // Not through tokio's readiness: the driver never waits to write,
        // and a socket tokio has not yet seen writable is not full.
        match self.state.try_send((&self.socket).into(), &t) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(
                    "WebRTC call: a datagram from {} to {destination} was not sent: {e}",
                    self.local
                );
                false
            }
        }
    }

    /// `addr` as this socket speaks it: an IPv4 address mapped into IPv6
    /// (`::ffff:a.b.c.d`) on an IPv6 socket.
    pub fn to_family(&self, addr: SocketAddr) -> SocketAddr {
        match (self.local, addr.ip()) {
            (SocketAddr::V6(_), IpAddr::V4(v4)) => {
                SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), addr.port())
            }
            _ => addr,
        }
    }
}

/// `addr` with an IPv4-mapped IPv6 address made IPv4 (`::ffff:a.b.c.d` →
/// `a.b.c.d`), as str0m's candidates and the POST's local address are.
pub(super) fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device group's rule, set on the socket whatever the system's
    /// default: `[::]` takes IPv4 too, `[::1]` does not.
    #[tokio::test]
    async fn the_device_listeners_rule_sets_the_ipv6_stack_itself() {
        let only_v6 = |s: &UdpSock| socket2::SockRef::from(&s.socket).only_v6().unwrap();
        let any = bind(SockId(1), "[::]:0".parse().unwrap(), V6::DualOnWildcard)
            .await
            .unwrap();
        assert!(!only_v6(&any), "[::] is dual-stack");
        let lo = bind(SockId(2), "[::1]:0".parse().unwrap(), V6::DualOnWildcard)
            .await
            .unwrap();
        assert!(only_v6(&lo), "[::1] is IPv6 alone");
        // An IPv4 datagram reaches the dual-stack socket, from IPv4.
        let from = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        from.send_to(b"x", ("127.0.0.1", any.local.port())).unwrap();
        let mut buf = vec![0u8; RECV_BUF];
        let mut got = Vec::new();
        // A datagram that never arrives fails the test, not hangs it.
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while got.is_empty() {
                std::future::poll_fn(|cx| any.poll_ready(cx)).await;
                any.recv(&mut buf, &mut got).unwrap();
            }
        })
        .await
        .expect("the IPv4 datagram reached the dual-stack socket");
        assert_eq!(got[0].source, from.local_addr().unwrap());
    }

    #[tokio::test]
    async fn a_wildcard_socket_serves_its_family_and_maps_ipv4_on_ipv6() {
        let v6 = bind(SockId(1), "[::]:0".parse().unwrap(), V6::System)
            .await
            .unwrap();
        let lo4: IpAddr = "127.0.0.1".parse().unwrap();
        let lo6: IpAddr = "::1".parse().unwrap();
        assert!(serves(v6.local, lo4) && serves(v6.local, lo6));
        let mapped = v6.to_family(SocketAddr::new(lo4, 5));
        assert_eq!(mapped.to_string(), "[::ffff:127.0.0.1]:5");
        assert_eq!(canonical(mapped), SocketAddr::new(lo4, 5));

        let v4 = bind(SockId(2), "0.0.0.0:0".parse().unwrap(), V6::System)
            .await
            .unwrap();
        assert!(serves(v4.local, lo4) && !serves(v4.local, lo6));
        let concrete = bind(SockId(3), "127.0.0.1:0".parse().unwrap(), V6::System)
            .await
            .unwrap();
        assert!(serves(concrete.local, lo4));
        assert!(!serves(concrete.local, "127.0.0.2".parse().unwrap()));
    }
}
