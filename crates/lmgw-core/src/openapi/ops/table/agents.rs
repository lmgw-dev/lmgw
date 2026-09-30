//! `ops-agents`: the agent catalog (api-docs design §4.7, agent-catalog
//! design §5), dispatched off `web/api_agents.rs`'s `op`.

use lmgw_api_types as dto;

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-agents";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "agent_set",
        tag: TAG,
        summary: "Install or replace an agent from a manifest",
        description: None,
        tool: Some("lmgw__agent_set"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::AgentImportReport>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_duplicate",
        tag: TAG,
        summary: "Copy an agent's manifest under a new id",
        description: Some(
            "Same manifest under a new id, config copied minus secrets — a copy must not \
             silently inherit a credential.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_duplicate),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_config_set",
        tag: TAG,
        summary: "Save a sparse patch over an agent's stored config",
        description: Some(
            "A sparse patch over the stored config values. `clear` is the way out of the \
             patch's one asymmetry: an empty submission keeps a stored secret (the house \
             convention for tokens), so removing a value — including clearing a secret — is \
             its own explicit list, which can also reset any field to its schema default.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_config_set),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_install",
        tag: TAG,
        summary: "Install an agent from an OCI image carrying its manifest",
        description: Some(
            "Install an agent from an image: lmgw reads its manifest out of the image at \
             /lmgw/agent.json without starting it, then installs it through the same path \
             agent_set uses, so the report shape is the same (plus image, digest, pulled and \
             manifest_path). `pull` defaults to 'never' — an image not already on the box is \
             reported rather than downloaded.",
        ),
        tool: Some("lmgw__agent_install"),
        args: OpArgs::Tool,
        response: Resp::Json(|g| g.root_schema_for::<dto::AgentImportReport>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_pull",
        tag: TAG,
        summary: "Fetch this agent's image again and report whether it moved",
        description: Some(
            "Pressing this is the consent, so it pulls under every policy, 'never' included. \
             When the digest moved, the new image's manifest is read and compared with the \
             stored one — never adopted automatically; that is what agent_reimport is for.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_id_only),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_reimport",
        tag: TAG,
        summary: "Re-run the install path over an existing agent's row",
        description: Some(
            "Run the install path again over an existing row: the manifest is replaced and the \
             config is kept. Refused when the image carries a different agent id than this row \
             — install it as its own agent instead.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_id_only),
        response: Resp::Json(|g| g.root_schema_for::<dto::AgentImportReport>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_dev_url_set",
        tag: TAG,
        summary: "Point an agent's app at a dev server, or back at its image",
        description: Some(
            "A row setting, never part of the manifest: overrides service mode only. Setting \
             one stops a running app container, since it was started from the image this \
             override replaces. Omitting url (or an empty string) clears it back to the image.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_dev_url_set),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_enable",
        tag: TAG,
        summary: "Enable or disable an agent",
        description: Some(
            "Disable is the kill switch: the agent's token stops authenticating anywhere, and \
             a running app container is stopped.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_enable),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_delete",
        tag: TAG,
        summary: "Remove an agent from the catalog",
        description: None,
        tool: Some("lmgw__agent_delete"),
        args: OpArgs::Tool,
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_reset",
        tag: TAG,
        summary: "Restore a shipped agent to its built-in manifest",
        description: Some(
            "A shipped agent back to its built-in manifest, keeping its config — except a \
             value the new schema no longer declares, which is dropped and named in the \
             answer.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_id_only),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_open_chat",
        tag: TAG,
        summary: "Open a chat thread for a chat-kind agent",
        description: Some(
            "Open a new Chat thread configured from a `chat`-kind agent's manifest — model \
             alias, system prompt, attached MCP tools — optionally patched by `values` for this \
             thread only.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_open_chat),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_run",
        tag: TAG,
        summary: "Start one phase of a batch agent's run",
        description: None,
        tool: Some("lmgw__agent_run"),
        args: OpArgs::Hand(args::agent_run),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: Some(r#"{"id":"mail-labeler","phase":"apply","base_job":1234}"#),
    },
    OpDoc {
        name: "agent_run_cancel",
        tag: TAG,
        summary: "Ask an agent's in-flight run to stop",
        description: Some(
            "Cooperative cancel: the executor stops between items and keeps the rows it has.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_id_only),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_token_get",
        tag: TAG,
        summary: "Read an agent's own MCP/API token",
        description: Some(
            "A read of the column that already holds the value, not a reveal-once: lmgw hands \
             this token to the agent's container on every run, including one it did not start. \
             Mints the token first if the agent has none yet, and re-derives its scope from the \
             manifest either way.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_id_only),
        response: Resp::OpOutcome,
        // Review R2 #8: `token::ensure` mints a missing token and always
        // rewrites the token's scope — a write, even when nothing changed.
        writes: true,
        reveals_secret: true,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_token_rotate",
        tag: TAG,
        summary: "Mint a new token for an agent, invalidating the old one",
        description: Some(
            "Replaces the agent's token in one write. A running service container is holding \
             the old one, so it is stopped — the next request to the agent's origin starts it \
             again with the new token.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_id_only),
        response: Resp::OpOutcome,
        writes: true,
        // The answer carries the new plaintext `token`.
        reveals_secret: true,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_service_start",
        tag: TAG,
        summary: "Start an agent's app container",
        description: Some(
            "The same on-demand path a proxied request takes, pressed by hand: one start \
             however many callers ask, the manifest's health probe, its start_timeout_seconds.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_id_only),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_service_stop",
        tag: TAG,
        summary: "Stop an agent's app container",
        description: Some(
            "The stop ladder. Stopping something that is not running is success — \"make sure \
             this is not running\" is what the button means.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_id_only),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agent_service_log",
        tag: TAG,
        summary: "Read an agent's app container's log tail",
        description: Some(
            "The App tab's log block, as long as the reader asks for — `lines` 0 means the \
             whole log. The only account of a detached service container: it writes no run \
             ledger, no job row and no result.",
        ),
        tool: None,
        args: OpArgs::Hand(args::agent_service_log),
        response: Resp::OpOutcome,
        writes: false,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "agents_restore",
        tag: TAG,
        summary: "Reinstall any shipped agent that was deleted",
        description: Some(
            "Every shipped agent not currently installed is put back with its built-in \
             manifest. An agent still installed, edited or not, is left alone.",
        ),
        tool: None,
        args: OpArgs::NoArgs,
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
];
