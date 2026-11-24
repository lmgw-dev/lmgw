//! A loopback port that refuses connections, for the unit tests.

/// A loopback port nothing answers on, for as long as the socket lives:
/// bound and never listened on, so a connect is refused, and no other socket
/// (another test's server, or another gate running on this machine) can be
/// given the port meanwhile. A port bound and closed again was free for
/// whoever bound next. Not a dropped `MockServer` either: wiremock pools
/// those and hands them to the next test. The integration binary has the same
/// helper in `tests/it/common`.
pub(crate) fn refusing_port() -> (u16, tokio::net::TcpSocket) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind(([127, 0, 0, 1], 0).into()).unwrap();
    (socket.local_addr().unwrap().port(), socket)
}
