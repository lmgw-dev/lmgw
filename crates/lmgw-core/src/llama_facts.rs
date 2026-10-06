//! What lmgw knows about the llama-server a chat send goes to (llama egress
//! design §3.2, §4.2): the resolver `gate::fit_chat` asks once per send, and
//! the background `/props` cache of **external** `llama_cpp` rows.
//!
//! - **Managed containers** carry their facts on the registry entry, read at
//!   start, climb and adoption (`runtime::registry::llama_props`); the
//!   resolver reads them through the request's hold.
//! - **External rows** are probed in the background, one probe per row at a
//!   time, keyed by (row, `base_url`) — and by upstream model too, only under
//!   a router's answer ([`FactsKey`](crate::llama_facts::FactsKey)). The
//!   first use answers unknown and never waits (decision 14: unknown means
//!   today's bytes). There is no TTL: four events say the facts may be stale
//!   — an edit of the row, a transport failure on it, its media refusals,
//!   and its Test button — and each drops what the row knew.
//!
//! Its own module rather than beside the reader in `egress::llama_cpp`: the
//! cache is gateway state (it lives on `AppState`, is fed by the request
//! path and shown on the Upstreams page), while the egress only speaks the
//! wire. And not in `catalog`, whose cache is per row with a TTL and answers
//! a different question (what models a row lists).

mod cache;
pub use cache::*;
mod probe;
pub use probe::*;
mod resolve;
pub use resolve::*;
mod observe;
pub use observe::*;
mod view;
pub use view::*;

#[cfg(test)]
mod tests;
