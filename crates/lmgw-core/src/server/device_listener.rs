//! The device listener (device link design §4): a TLS 1.3 listener of its
//! own beside the plain one, where paired devices authenticate by client
//! certificate.
//!
//! - **`bind`**: where it listens (`devices.listen`, `devices.port`) and
//!   its sockets: the loopback pair, `[::]` dual-stack, or one address;
//! - **`tls_listener`**: [`TlsListener`], which owns those sockets and an
//!   accept task per socket, aborted when it is dropped or closed, so a move
//!   rebinds the same port at once (§4.3);
//! - **`connection`**: one connection, off the accept loop: the server
//!   certificate and the handshake's configuration picked by the local
//!   address and the pending tokens, the handshake under
//!   `devices.handshake_timeout_s`, the client certificate read once
//!   (`Conn`), and the HTTP/1 serving with `devices.header_timeout_s` on
//!   each request head and on a kept-alive connection's wait for the next;
//!   a connection without a certificate bounded as a whole by
//!   `devices.unauthenticated_timeout_s`, each of its answers carrying
//!   `Connection: close`;
//! - **`runner`**: [`DeviceListener`], what the gateway holds of it: started
//!   while the gateway serves and `devices.listener` is on, moved or stopped
//!   at once when its settings change, each stop ending its own streams,
//!   sessions and calls only (`Stops`, §4.4), with the status and the log
//!   line.

mod bind;
mod connection;
/// The connections without a certificate yet, bounded (§4.3).
mod preauth;
mod runner;
mod tls_listener;

pub use bind::Plan;
pub use preauth::peer_key;
pub use runner::{Applied, Bound, DeviceListener, OpenedCa};
pub use tls_listener::TlsListener;
