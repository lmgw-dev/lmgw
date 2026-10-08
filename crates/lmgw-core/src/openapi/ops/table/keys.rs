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
        summary: "Create a client, owner or device API key",
        description: Some(
            "Create a new API key: a client key (default), an owner key or a device key. The \
             plaintext is returned once for a client key; an owner key's plaintext stays \
             retrievable with key_reveal for as long as the row exists. A client key may be \
             created with its alias scope and MCP tool scope already set (scope_mode, \
             scope_patterns, tool_scope_mode, tool_scope_patterns — the same fields and rules \
             as key_set); an owner key refuses a scope. A device key (kind 'device', named \
             device:<name>) pairs one client app: it is created with its whole policy — the \
             scopes, budget, limits and expiry key_set takes — an optional hosts_label and an \
             optional self_admin level (off by default), is \
             stored as a hash only, and answers {key, link}: the key once, and the pairing link \
             lmgw-pair:?v=1&url=…&name=…&key=… that carries it to the device (url is the \
             request's, or this gateway's primary address). A device holds the Chat API on top \
             of a client key's inference, and nothing of the admin plane.",
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
            "The settable half of one key's policy. Sparse like every other patch: a field \
             left out keeps the value the row already has. The other half of a key's policy is \
             derived (an agent token's scope and enabled flag are written by lmgw from the \
             agent's manifest and row) and is refused here rather than accepted and silently \
             taken back by the next resync. A device key also takes hosts_label (\"\" clears \
             it) and self_admin, its level of lmgw's admin tools: off, read_only or full, capped \
             by the self-admin level in Settings; what the device sees follows the capped \
             level. Raised above off (under a self-admin level above off), the device's change feed \
             receives the threads and folders with the self-admin toolset as created; set to \
             off, as deleted, its Chat turns on such a thread stop, and its realtime sessions \
             bound to such a thread close with 4004. Lowered to read_only, a turn still running \
             is refused its next write tool. On any change its change feed sends a state event \
             with self_admin, what its admin tools may do now, and its realtime sessions that \
             carry the lmgw label list it again. Disabling a key of any \
             kind, or its expires_at passing, ends what it holds \
             open at once: its realtime sessions close with 4003, its Chat streams end with an \
             error frame whose code is revoked, its change feed with a revoked event, and its \
             /mcp notification stream ends.",
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
             \"closed\": the next start seeds it again, disabled, until it is enabled \
             — the credential handed out until then is gone. Deleting a key ends what it holds \
             open at once, as disabling it does.",
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
            "The stored plaintext of one owner-kind key. A client key and a device key are \
             hashed and shown only once at creation, so there is nothing to reveal for them (a \
             device is rotated to pair it again); an agent token is read from the agent's own \
             page instead.",
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
        summary: "Mint a new plaintext for an owner or device key, invalidating the old one",
        description: Some(
            "Replace an owner key's hash and plaintext in one write, so the old value stops \
             working the moment the new one is live. A disabled owner:self-admin key stays \
             disabled after rotation — the plane stays closed. On a device key it re-pairs: a \
             new key on the same row (its policy and history stay), answered with a new {key, \
             link}. Either way, what the old value holds open ends at once, as on a disable: \
             realtime sessions close with 4003, Chat streams end with an error frame whose code \
             is revoked, the change feed with a revoked event, and /mcp notification streams \
             end.",
        ),
        tool: None,
        args: OpArgs::Hand(args::key_rotate),
        response: Resp::OpOutcome,
        writes: true,
        // The answer carries the new plaintext `key` (review R2 #8).
        reveals_secret: true,
        confirm_note: Some(
            "rotating owner:dashboard signs every browser out and ends the dashboard's running \
             Chat turn and voice session; rotating any other key ends what it holds open — a \
             device's until it is paired again",
        ),
        deprecated: false,
        example: None,
    },
];
