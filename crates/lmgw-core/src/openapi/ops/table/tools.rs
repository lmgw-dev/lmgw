//! `ops-tools`: southbound MCP servers and the per-tool switch (api-docs
//! design §4.7).

use lmgw_api_types as dto;

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-tools";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "mcp_server_set",
        tag: TAG,
        summary: "Create, update, delete, enable, disable or test a southbound MCP server",
        description: Some(
            "Every action answers the server's id and a sentence. `test` connects to the \
             server and also answers how many tools it lists (`tool_count`); a server that \
             cannot be reached is an error answer instead.",
        ),
        tool: Some("lmgw__mcp_server_set"),
        args: OpArgs::Struct(|g| g.root_schema_for::<crate::ops::McpServerPatch>()),
        response: Resp::Json(|g| g.root_schema_for::<dto::McpServerSaved>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "tool_set",
        tag: TAG,
        summary: "Enable or disable one discovered tool",
        description: Some(
            "Flip a tool's per-tool switch. Disabling requires the tool to be offered right \
             now — a typo would otherwise persist a switch for a name that does not exist. \
             Enabling never checks, which is how a stale row is cleared.",
        ),
        tool: None,
        args: OpArgs::Hand(args::tool_set),
        response: Resp::Json(|g| g.root_schema_for::<dto::MessageAck>()),
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: Some(r#"{"name":"gh__search","enabled":false}"#),
    },
];
