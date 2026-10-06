//! The request gate — unified-KV spec §3.3/§5, ladder spec §3.2–3.4.
//!
//! Both specs describe **one async gate** in front of every local chat send
//! (unified-KV design §5): the hold swap, the candidate pick, the ladder's fit
//! check and climb, the unified-KV pool reservation, then admission itself.
//! It is one module with two halves of one pipeline, run in the order 1, 2, 5
//! and then 3, 4:
//!
//! - [`open`] — the **per-request** half: the candidate pick ([`candidate`]:
//!   a candidate alias's walk over its models), the hold swap for direct
//!   model names ([`crate::config::Snapshot::resolve_for_request`]), the
//!   site's own route check, and admission (`vram::admit_or_external`, with
//!   §4.7's outside-VRAM fallback). Once per request, before anything is
//!   sent.
//! - [`fit`] — the **per-send** half: the max-output clamp, the prompt count
//!   on the running server and the pool reservation ([`pool`]). Immediately
//!   before every chat send to a local model, releasing with that send's
//!   response.
//! - [`send`] — the send itself, with the lease the fit returned: unchanged
//!   for every row without a ladder; on a ladder, the exact count beside the
//!   forward, the answer held until its verdict, and the climb ([`ladder`])
//!   when the request does not fit the rung that runs.
//!
//! Why the order is not the spec's list order: the count needs the *running*
//! server (ladder design §3.3 puts the fit check "after acquire, before
//! forwarding"), and the in-process runners hold one admission across many
//! turns, each of which reserves and releases its own tokens — a reservation
//! must never live as long as the hold. [`fit`]'s module doc has the whole
//! argument.
//!
//! The pieces both halves share are built once, here ("The clamp and the
//! counting are shared code" — unified-KV design §5):
//!
//! - [`clamp`] — the max-output clamp (ladder design §3.2, unified-KV design
//!   §3.3 step 1): bind a request's `max_tokens` to the row's `n_predict`
//!   ceiling before it ever reaches egress.
//! - [`count`] — the prompt count (ladder design §3.3, unified-KV design §3.3
//!   step 2): `/apply-template` then `/tokenize` against the running server,
//!   plus the per-image token bound a projector needs to be counted at all.
//! - [`facts`] — what the gate reads about a container: the row it was
//!   *started* with, captured on the registry entry and read through the
//!   hold, never the row as it has been edited since (second review, finding 1).
//! - [`tool_images`](crate::gate::tool_images) — whether a send's
//!   tool-result images may go to a llama.cpp server as images (llama egress
//!   design §8.2's predicate), decided once per send in
//!   [`fit`](crate::gate::fit) and carried on its lease, and how many would
//!   ([`tool_media`](crate::gate::tool_media)).
//! - [`fallback_images`] — a request's images on a fallback that cannot see
//!   them: placeholders, decided once per send at the top of
//!   [`fit`](crate::gate::fit) (the owner's ruling, 2026-10-06: a configured
//!   fallback is always used).
//!
//! Rows the gate does not apply to — not guarded, not (later) a ladder, or
//! not local at all — pass through both halves exactly as they did before it
//! existed: no `/apply-template` call, the same request body, no new header
//! (unified-KV design §7 item 14).

pub mod candidate;
pub mod clamp;
pub mod count;
pub mod facts;
pub mod fallback_images;
pub mod fit;
pub mod ladder;
pub mod open;
pub mod pool;
pub mod send;
pub mod tool_images;

pub use candidate::{legacy_facets, request_facets, CandidateCtx};
pub use clamp::clamp_max_tokens;
pub use count::{
    count_chat_prompt, count_text_prompt, image_token_bound, media_parts, ImageBound, MediaParts,
    ProjectorRow, PromptCount,
};
pub use facts::GateFacts;
pub use fit::{attribute, fit_chat, fit_text, planned_clamp, planned_rung, FitRefusal, TurnLease};
pub(crate) use fit::{guard_facts, on_running_server};
pub use ladder::{RungTag, RUNG_HEADER};
pub use open::{
    open, open_pinned, resolve, usable_fallback, AdmissionPolicy, FallbackReason, GateHeaders,
    OpenFailed, Opened, RouteCheck, Routed,
};
pub(crate) use send::rebuild;
pub use send::{send_gated, send_gated_marked, CountInput, Sent};
pub use tool_images::{tool_image_predicate, tool_media, ToolImageInputs};
