//! Hand-written `/v1` schemas and examples (api-docs design §4.8): what
//! lmgw's own parsers read and its serializers emit, not provider docs.
//! Split per §3.2's file list — `chat`, `responses`, `anthropic`, `aux`,
//! `media`, `models`, `errors`, `mcp` — each `pub(crate)` so
//! `planes::inference` (a sibling under `openapi`, not a descendant of this
//! module) and `build`/`schemas` (the same) can reach the `SchemaFn`s and
//! example functions directly.

pub(crate) mod anthropic;
pub(crate) mod aux;
pub(crate) mod chat;
pub(crate) mod errors;
pub(crate) mod mcp;
pub(crate) mod media;
pub(crate) mod models;
pub(crate) mod responses;
