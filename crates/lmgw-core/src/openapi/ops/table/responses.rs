//! `ops-responses`: stored `/v1/responses` chains (api-docs design §4.7).

use super::{OpArgs, OpDoc, Resp};
use crate::openapi::ops::args;

const TAG: &str = "ops-responses";

pub(super) const OPS: &[OpDoc] = &[
    OpDoc {
        name: "response_chain_delete",
        tag: TAG,
        summary: "Delete every stored response of one conversation chain",
        description: Some("Delete every stored response sharing one chain_id."),
        tool: None,
        args: OpArgs::Hand(args::response_chain_delete),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: None,
    },
    OpDoc {
        name: "responses_gc",
        tag: TAG,
        summary: "Evict stored responses by the retention settings, or clear all of them",
        description: Some(
            "Evict stored responses now: 'rules' (default) applies the retention settings \
             (responses_retention_hours, responses_max_chains); 'all' clears every stored \
             response regardless of age.",
        ),
        tool: None,
        args: OpArgs::Hand(args::responses_gc),
        response: Resp::OpOutcome,
        writes: true,
        reveals_secret: false,
        confirm_note: None,
        deprecated: false,
        example: Some(r#"{"scope":"rules"}"#),
    },
];
