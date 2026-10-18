//! What a client of lmgw needs besides its I/O (client-apps design §4.1):
//! the rules the dashboard's voice panel and every other client run alike.
//!
//! - [`realtime`]: the realtime protocol of a session bound to a Chat
//!   thread — the server events parsed, the client events built — on a
//!   WebSocket or a WebRTC call, with the call's close reasons and the
//!   parts an oversized event travels in;
//! - [`voice`]: the voice state derived from those events, and the
//!   truncate-first rule;
//! - [`truncate`]: each reply's audio, and the truncate that keeps what was
//!   heard when one is cut, from the player's cursor;
//! - [`feed`]: the Chat change feed — an SSE decoder, the typed events, the
//!   cursor to resume from;
//! - [`mcp_host`]: a device's MCP host link — its refusals, its closes,
//!   and the `_meta` lmgw stamps on the calls it forwards;
//! - [`mcp_apps`]: MCP Apps on `/mcp` — the revision lmgw follows, the
//!   extension's identifiers, the Chat `tool` ready and result frames a
//!   host reads;
//! - [`requests`]: the requests of the routes a client uses, and readers of
//!   their answers, the device link's enrolment among them;
//! - [`pairing`]: the device link's pairing link v2, what it pins and
//!   carries, the device's URLs and their order;
//! - `tls` (feature `tls`): the device link's pinned TLS, the enrolment's
//!   verifier and the client configuration every lmgw-facing connection
//!   is built from, with the app's crypto provider;
//! - [`ongoing`]: which Chat folder an ongoing conversation lives in, the
//!   rule every client runs alike;
//! - [`base64`]: the audio's encoding.
//!
//! **Sans-IO.** Nothing here opens a socket, reads a clock or spawns a
//! task: bytes and times go in, events and requests come out. It builds for
//! `wasm32-unknown-unknown` and natively. Its public types are owned, for a
//! later FFI wrapper. A few getters lend what they hold (`&str`, `&Reply`)
//! rather than clone it, on paths a client calls per audio frame; an FFI
//! object keeps this crate's state behind its own lock and clones what it
//! hands over. The wire types are `lmgw-api-types`', re-exported as
//! [`types`] so a client depends on one version of both.
//!
//! **Forward compatible.** A gateway newer than the client adds events and
//! enum values. Every enum read from the wire has an `Unknown` fallback and
//! is `#[non_exhaustive]`: a value the client does not know reads (kept as
//! sent where a client may write it back), and an event it does not know
//! comes as `Unknown` with its data, so nothing stops a client and nothing
//! is dropped without a word.

pub use lmgw_api_types as types;

pub mod base64;
mod der;
pub mod feed;
pub mod mcp_host;
pub use lmgw_api_types::mcp_apps;
pub mod ongoing;
pub mod pairing;
pub mod realtime;
pub mod requests;
mod rfc3339;
#[cfg(test)]
mod test_pki;
// The crate's own tests build it with or without the feature, so the
// workspace's test run checks the trust pin (Cargo.toml, `[features]`).
#[cfg(any(feature = "tls", test))]
pub mod tls;
pub mod truncate;
pub mod voice;
