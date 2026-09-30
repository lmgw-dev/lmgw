//! `ops-keys`: the credential ops (api-docs design §4.7, principals design
//! §3.12). None of these has a self-admin tool — the self-admin plane has
//! never exposed a credential, and `key_reveal`/`key_rotate` hand out the key
//! that holds every capability.

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-keys";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "key_create",
        tag: TAG,
        summary: "Create a client or owner API key",
        description: Some(
            "Create a new API key: a client key (default) or an owner key. The plaintext is \
             returned once for a client key; an owner key's plaintext stays retrievable with \
             key_reveal for as long as the row exists. A client key may be created with its \
             alias scope and MCP tool scope already set (scope_mode, scope_patterns, \
             tool_scope_mode, tool_scope_patterns — the same fields and rules as key_set); an \
             owner key refuses a scope.",
        ),
        tool: None,
        args: OpArgs::Hand(args::key_create),
        response: Resp::OpOutcome,
        writes: true,
        // The answer carries the new key's `plaintext` (review R2 #8).
        reveals_secret: true,
        confirm_note: None,
        deprecated: false,
        example: Some(
            r#"{"name":"ci-bot","tool_scope_mode":"allow","tool_scope_patterns":"docs__*"}"#,
        ),
    },
    OpDoc {
        name: "key_set",
        tag: TAG,
        summary: "Change a key's policy — scope, budget, rate limits, expiry",
        description: Some(
            "The owner-set half of one key's policy. Sparse like every other patch: a field \
             left out keeps the value the row already has. The other half of a key's policy is \
             derived (an agent token's scope and enabled flag are written by lmgw from the \
             agent's manifest and row) and is refused here rather than accepted and silently \
             taken back by the next resync.",
        ),
        tool: None,
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::KeyPatch>()),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "key_delete",
        tag: TAG,
        summary: "Revoke an API key",
        description: Some(
            "Delete a key by id. Deleting the dashboard's own owner key is allowed and means \
             \"closed\": the next start seeds it again, disabled, until the owner enables it \
             — the credential handed out until then is gone.",
        ),
        tool: None,
        args: OpArgs::Hand(args::key_delete),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "key_reveal",
        tag: TAG,
        summary: "Show an owner key's stored plaintext",
        description: Some(
            "The stored plaintext of one owner-kind key. A client key is hashed and shown only \
             once at creation, so there is nothing to reveal for it; an agent token is read \
             from the agent's own page instead.",
        ),
        tool: None,
        args: OpArgs::Hand(args::key_reveal),
        response: Resp::OpOutcome,
        writes: false,
        reveals_secret: true,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "key_rotate",
        tag: TAG,
        summary: "Mint a new plaintext for an owner key, invalidating the old one",
        description: Some(
            "Replace an owner key's hash and plaintext in one write, so the old value stops \
             working the moment the new one is live. A disabled owner:self-admin key stays \
             disabled after rotation — the plane stays closed.",
        ),
        tool: None,
        args: OpArgs::Hand(args::key_rotate),
        response: Resp::OpOutcome,
        writes: true,
        // The answer carries the new plaintext `key` (review R2 #8).
        reveals_secret: true,
        confirm_note: Some("rotating owner:dashboard signs every browser out"),
        deprecated: false,
        example: None,
    },
];
