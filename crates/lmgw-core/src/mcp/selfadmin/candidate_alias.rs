//! `lmgw__candidate_alias_set` (candidate-aliases design §4.1, §4.6, §6) —
//! a child module of [`crate::mcp::selfadmin`] for the same file-size reason
//! [`crate::ops::candidate_alias`] is a child of [`crate::ops`]: this file
//! carries the tool's schema and long description; the parent keeps only the
//! one line that adds it to the catalog and the one match arm that dispatches
//! it (exactly like every other `*_set` tool already does).

use super::{bool_p, enum_p, str_p, Builtin};

pub fn tool() -> Builtin {
    Builtin {
        name: "lmgw__candidate_alias_set",
        writes: true,
        description:
            "Create, update, delete, enable or disable a candidate alias: a client-facing name \
             backed by a PRIMARY local chat model plus an ordered list of ALTERNATES, used only \
             when already loaded — the alias itself only ever starts the primary, never an \
             alternate. Resolution order: without background, the primary if loaded, else a \
             loaded alternate, else load the primary (evicting as usual), else the fallback \
             under the GPU hold or when VRAM lmgw cannot free is short. With background on, the \
             traffic is a GPU guest: the primary if loaded and not busy with the owner's work, \
             else startable without disturbing the owner, else a loaded alternate, else the \
             fallback — it never evicts anything the owner is using and never waits. One \
             fallback answers for the whole alias in both modes; a candidate's own row fallback \
             is never used through it. Capabilities are explicit: only the five facets \
             (vision, audio, tool_calls, reasoning, structured_output) every candidate supports \
             can be enabled, and they are on by default — capabilities_disabled switches one \
             off. Identify an existing alias by id or by its current alias name.",
        props: vec![
            (
                "action",
                enum_p(
                    "What to do. 'preview' validates and derives a draft (any of the fields \
                     below, over an existing row named by id/alias or a brand-new one) without \
                     saving it — the same response shape, plus 'addable' (local chat models not \
                     yet in candidates and which enabled facet each would drop) and an 'error' \
                     that never aborts the rest of the answer.",
                    &["create", "update", "delete", "enable", "disable", "preview"],
                ),
            ),
            ("id", super::int_p("Candidate alias row id.")),
            (
                "alias",
                str_p(
                    "The client-facing model name. On create this is the new alias; otherwise \
                     it selects the row (pass id as well to rename). Refused if it already \
                     names a plain alias or a local model of any class.",
                ),
            ),
            (
                "candidates",
                str_p(
                    "Local chat model ids (lmgw__local_model_set rows, not aliases), one per \
                     line or comma-separated. The FIRST is the primary — the only one the alias \
                     ever starts; the rest are alternates in preference order, used only when \
                     already loaded. Every id must name an enabled local chat model. On update, \
                     absent leaves the stored list unchanged; it can never be saved empty — \
                     delete the alias instead.",
                ),
            ),
            (
                "background",
                bool_p(
                    "Scheduled/background traffic: never evicts an owner-used model, never \
                     waits, and may only start the primary into free VRAM or evict another idle \
                     background-owned entry. Default false.",
                ),
            ),
            (
                "fallback_mode",
                enum_p(
                    "How this alias' one fallback resolves. 'inherit' reads the global GPU-hold \
                     fallback; 'none' refuses (503 gpu_hold / gpu_deferred) rather than leave \
                     the machine; 'alias' routes to 'fallback'. Default inherit.",
                    &["inherit", "none", "alias"],
                ),
            ),
            (
                "fallback",
                str_p(
                    "The alias this candidate alias falls back to when fallback_mode is \
                     'alias'. Must resolve, must not itself be local, and must not be another \
                     candidate alias.",
                ),
            ),
            (
                "capabilities_disabled",
                str_p(
                    "Facet names switched OFF, one per line or comma-separated: vision, audio, \
                     tool_calls, reasoning, structured_output. This is the FULL desired list of \
                     the owner's own switches, not a delta and never auto-filled — every facet \
                     not named here that every candidate currently supports ends up enabled, \
                     including one that becomes supported later (it turns on by itself unless \
                     named here). On create, nothing is refused: an unsupported facet is simply \
                     left off. On update, switching a facet back on (removing it from this list) \
                     or leaving an already-enabled facet unnamed while a candidate drops it is \
                     refused, naming the facet. Absent leaves the stored list unchanged; use \
                     clear=capabilities_disabled to reset it to empty (every common facet back \
                     on).",
                ),
            ),
            ("enabled", bool_p("Whether the alias resolves.")),
            ("notes", str_p("Free-text notes.")),
            (
                "clear",
                str_p(
                    "Field names to reset to unset, comma-separated: capabilities_disabled, \
                     notes.",
                ),
            ),
        ],
        required: &["action"],
    }
}
