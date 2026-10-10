//! DTOs shared between the lmgw admin JSON API (`/api/…` in lmgw-core) and the
//! Leptos UI (lmgw-ui). Everything here is `serde` round-trippable and carries
//! no server-side logic; the server converts its internal state into these
//! views at the API boundary.
//!
//! Grows with the API, one domain module at a time.
//!
//! [`image_lab`] is one of the modules that carry a function rather than only a
//! shape: its builder is the Image lab's request contract, and *both* sides
//! have to produce the same bytes from it (image-generation design §8).
//!
//! [`bench_compare`] is the second: the regression rule, which the ops apply
//! to a run and its previous comparable one and the Benchmarks page to any
//! runs the owner puts side by side.
//!
//! [`chat_voice`] is a third: the language hint's shape and the
//! turn-detection names, which the gateway checks and the dashboard says
//! before Save.
//!
//! [`builds`] is shared rather than mirrored: the server stores exactly those
//! shapes, so there is no server-side struct for a mirror to drift from.

/// The audio CPU switch's run-args rule ([`audio_engine::strip_gpu_devices`],
/// [`audio_engine::cpu_run_args`]), shared so the gateway and the audio
/// editor strip the same flags and warn about the same ones.
pub mod audio_engine;
/// Benchmark runs (benchmark design §4–§7): the suite's parameters, the
/// measured points, probes, timeline and identity — the shapes `bench_runs`
/// stores as JSON columns. A namespace of its own, like [`builds`], because
/// its names (`Phase`, `Stat`, `Energy`) only make sense inside it.
pub mod bench;
/// The comparison rule (benchmark design §6): comparable or not, the
/// headline numbers, the regression verdicts. Shared, like [`image_lab`]'s
/// builder, so the ops and the Benchmarks page judge runs the same way.
pub mod bench_compare;
/// The benchmark ops' arguments and answers (benchmark design §8.1), beside
/// [`bench`]'s stored shapes.
pub mod bench_ops;
pub mod builds;
/// `CandidateAliasView` (candidate-aliases design §4.1) — its own module
/// because the derived half alone is a dozen-plus fields.
pub mod candidate_alias;
// The client-apps design record, §4.3.
/// The Chat API's thread and folder shapes: the list rows, an open thread,
/// a folder, what a folder create takes — the types the gateway serializes
/// and the API document is generated from.
pub mod chat;
/// MCP approvals in the Chat (client-apps design §6).
pub mod chat_approvals;
/// The Chat's attachment routes: the upload's query, a PDF's mode, a new
/// transcription.
pub mod chat_attachments;
/// The Chat export: its query and the `lmgw.chat.v1` JSON file.
pub mod chat_export;
// The client-apps design record, §2.
/// The Chat change feed's events and cursor: what `GET /chat/api/feed`
/// sends, a namespace of its own like [`chat_voice`].
pub mod chat_feed;
// The client-apps design record, §3.
/// Ongoing-conversation folders: a folder's `ongoing` field and
/// `POST /chat/api/folders/{id}/current`.
pub mod chat_folders;
/// The frames of the Chat API's event streams, one type per event name.
pub mod chat_frames;
// The personality-profiles design record, §1 and §3.1.
/// The Audio lab's shapes: its model list, the voice library of reference
/// clips and the transcripts written for them.
pub mod audio_lab;
/// Personality profiles: `/chat/api/profiles*`'s shapes and the examples'
/// text form, a namespace of its own like [`chat_voice`].
pub mod chat_profiles;
/// A thread's whole read, its settings and message actions, and the search
/// (`GET /chat/api/threads/{id}` and the routes that act on a thread).
pub mod chat_threads;
/// The bodies of the streaming Chat routes: send, continue, regenerate,
/// voice warm-up.
pub mod chat_turn;
/// The Chat's voice rules (chat-voice design §2): turn-detection names and
/// labels, the language hint's shape — checked by the gateway and said
/// before Save by the dashboard.
pub mod chat_voice;
pub mod image_lab;
/// The Knowledge bases API: bases, files, uploads, jobs' answers, the source
/// viewer and the search playground.
pub mod knowledge;
// The client-apps design record, §5.
/// MCP Apps on `/mcp` (§7): the revision followed, its identifiers, and
/// the Chat `tool` result frame's fields a host reads.
pub mod mcp_apps;
/// The device MCP host link (`GET /mcp/host`): its refusals, its close
/// codes, and the `_meta` lmgw stamps on the calls it forwards over it.
pub mod mcp_host;
/// OpenAPI extension-key names (api-docs design §4.4), read by both the
/// document builder (lmgw-core) and the page that renders it (lmgw-ui).
pub mod openapi_ext;
/// `GET /v1/realtime`'s settings section and the `realtime_budget` op
/// (realtime design §12) — a namespace of its own, like [`bench_ops`].
pub mod realtime;
pub mod scope;

pub use candidate_alias::CandidateAliasView;

mod container;
pub use container::*;
mod model_test;
pub use model_test::*;
mod model_detail;
pub use model_detail::*;
mod model_reads;
pub use model_reads::*;
mod ack;
pub use ack::{Ack, MessageAck};
mod status;
pub use status::*;
mod connect;
pub use connect::*;
mod models;
pub use models::*;
mod upstreams;
pub use upstreams::*;
mod responses;
pub use responses::*;
mod settings;
pub use settings::*;
mod mcp;
pub use mcp::*;
mod tools;
pub use tools::*;
mod downloads;
pub use downloads::*;
mod jobs;
pub use jobs::*;
mod audio_specs;
pub use audio_specs::*;
mod image_recipes;
pub use image_recipes::*;
mod wiring;
pub use wiring::*;
mod docs;
pub use docs::*;
mod billable_units;
pub use billable_units::*;
mod usage;
pub use usage::*;
mod chat_changed;
pub use chat_changed::*;
mod admin_level;
pub use admin_level::*;
mod agents;
pub use agents::*;
