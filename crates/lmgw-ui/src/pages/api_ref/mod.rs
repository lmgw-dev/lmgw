//! The API reference page (api-docs design §6): `/api-reference`, rendered
//! natively in Leptos from the gateway's own `GET /api/openapi.json`, with a
//! built-in request tester. Split into the doc model (`doc.rs`, pure), the
//! documentation column (`detail.rs`, `schema_view.rs`), the rail
//! (`rail.rs`), and the tester (`identity.rs`, `example.rs`, `draft.rs` —
//! its state and the request built from it —, `tester.rs`, `send.rs`,
//! `stream.rs`, `response_view.rs`, `json_view.rs`, `curl.rs`).

mod curl;
mod detail;
mod doc;
mod draft;
mod example;
mod identity;
mod json_view;
mod page;
mod rail;
mod response_view;
mod schema_view;
mod send;
mod stream;
mod tester;

pub use page::ApiReference;
