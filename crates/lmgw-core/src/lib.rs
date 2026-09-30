//! lmgw-core — self-hosted LLM API gateway library.
//!
//! Everything except the Tauri shell lives here: ingress parsing, the
//! canonical IR, the alias router, egress adapters, the proxy engine,
//! persistence, telemetry, the per-model container runtime, and the Axum
//! router factory.

pub mod agent;
/// The agent catalog (agent-catalog design): manifests, the store behind them,
/// and the built-ins that ship embedded. Distinct from [`agent`], which is the
/// server-side *tool loop* one of these manifests eventually drives.
pub mod agents;
pub mod audio;
/// Container builds from git — the Backends page (container-builds design):
/// build definitions, their runs, validation, tags and the builds directory.
pub mod backends;
/// The benchmark engine (benchmark design, WP1): the suite's corpus, points,
/// phases, probes and GPU sampler, driven against a running llama-server.
/// Starting that server, the GPU lease and the store are the integration's.
pub mod bench;
/// Candidate-alias derivation (candidate-aliases design §4.6): the facet
/// vocabulary, the positive-support rule, and the async fold over every
/// candidate's and the fallback's live capabilities that the alias editor,
/// `/v1/models`/`lmgw__models` and the gate worker's pick all read.
pub mod candidates;
pub mod capabilities;
pub mod catalog;
pub mod config;
pub mod egress;
pub mod error;
pub mod extract;
/// The request gate (unified-KV design §3.3/§5, ladder design §3.2–3.4): one
/// module with a per-request half (the hold swap, the candidate pick, the
/// site's route check, admission) and a per-send half (the max-output clamp,
/// the prompt count, the ladder's climb, the unified-KV pool reservation), in
/// front of every local chat send. The clamp and the count are built once
/// here so the ladder's fit check and the pool ledger call the same code
/// instead of drifting into two copies.
pub mod gate;
pub mod gguf;
pub mod hf;
pub mod image_recipes;
pub mod ingress;
pub mod ir;
pub mod jobs;
/// Knowledge bases (chat-complete design §9): the owner's documents, chunked,
/// embedded and searched with quickdoc-core's hybrid pipeline, in their own
/// private `knowledge.db`.
pub mod knowledge;
/// The ladder's own config shape and pure derivations (ladder design §4.1,
/// §4.2): `Rung`, and the per-rung view — per-slot context, switchover — every
/// other package (runtime, gate, capabilities, UI) reads instead of
/// recomputing.
pub mod ladder;
pub mod llama_caps;
pub mod mcp;
pub mod modelinfo;
pub mod net;
/// The OpenAPI 3.1 description of lmgw's own HTTP API (api-docs design), built
/// from `server::CAPABILITY_TABLE`, the op dispatcher and `schemars` schemas
/// of the api-types DTOs, rather than hand-maintained. Served at
/// `GET /api/openapi.json` (everything) and `GET /v1/openapi.json` (the
/// inference plane only).
pub mod openapi;
pub mod ops;
pub mod policy;
pub mod pricing;
/// The principal vocabulary and the credential resolver (principals design
/// §3): who a request is, what a route needs, and how the two meet.
pub mod principal;
pub mod proxy;
pub mod quickdoc;
pub mod responses;
pub mod runtime;
pub mod sdcpp_caps;
pub mod server;
pub mod sse;
pub mod state;
pub mod store;
pub mod telemetry;
pub mod update;
pub mod vram;
pub mod web;
