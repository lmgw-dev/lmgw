//! The providers' own schemas check what lmgw puts on their wires (api-compat
//! design, level 1), in every test that drives an upstream or a `/v1` route.
//!
//! * (a) [`egress`]: every request body an egress builds — OpenAI chat and
//!   Responses, Anthropic messages and count tokens, Gemini generateContent,
//!   countTokens and batchEmbedContents, OpenAI-shaped embeddings — against
//!   the provider's pinned schema, strict about unknown keys. Hooked into
//!   lmgw-core's `egress::watch` for the whole test process at its start
//!   ([`install`]), so no test asks for it.
//! * (b) [`answers`]: every answer and stream event on the OpenAI- and
//!   Anthropic-shaped `/v1` routes, layered around the router
//!   [`super::serve_on`] serves.
//! * (c) the property test (`tests/it/api_compat/ir_props.rs`) pushes
//!   generated IR conversations through every egress and asserts on
//!   [`egress::check`] itself.
//!
//! The schemas are pinned in `tests/fixtures/api_schemas/` and refreshed by
//! `scripts/api-schemas-update.sh`. What lmgw sends on purpose beyond
//! OpenAI's schema to llama.cpp and compatible rows is [`extensions`]; what
//! it gets wrong and has not fixed yet is [`known`], printed whenever it
//! matches. Every other violation is printed at once and fails the test
//! process at its exit ([`ledger`]).

pub mod answers;
pub mod diagnose;
pub mod egress;
pub mod extensions;
pub mod known;
pub mod ledger;
pub mod schemas;

#[ctor::ctor]
unsafe fn install() {
    lmgw_core::egress::watch::install(egress::watched);
}
