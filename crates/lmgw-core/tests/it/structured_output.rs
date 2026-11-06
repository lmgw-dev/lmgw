//! Structured output, translated everywhere
//! (`docs/design/2026-10-22-structured-output-design.md`): every ingress
//! reads its dialect's format into the IR's one field (§5), every egress
//! writes it in its own (§6), the gate refuses a type a model is known not
//! to produce (§7), and each upstream's refusals are read as
//! `response_format_unsupported` / `invalid_json_schema` with `param` set
//! (§8, §9) — from the captured bodies under
//! `tests/fixtures/structured_output/`.

mod candidates;
mod classify;
mod early;
mod egress;
mod ingress;
mod llama;
mod routes;
