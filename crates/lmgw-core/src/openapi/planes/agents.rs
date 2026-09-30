//! `/api/agents/*`, the agent catalog (api-docs design §4.6). WP4.
//!
//! Three capabilities meet on this plane (`web/api_agents.rs`'s own module
//! doc): the owner's Admin reads and writes the catalog and its exports, an
//! agent's AgentSelf reads its own row and its own runs, and an agent's
//! Ledger opens and drives a run. The tag follows the capability, not the
//! path — `agent-runtime` is exactly "every AgentSelf and Ledger route"
//! (`tags.rs`), so `GET /api/agents/{id}` (AgentSelf: both the owner's detail
//! page and the container's own read of itself) sits there, not under
//! `agents`.

use schemars::generate::SchemaGenerator;
use schemars::Schema;

use lmgw_api_types as dto;

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use super::super::schemas;
use crate::web::api_agents::{ExportQuery, ImportQuery, OpenBody};

fn base(
    method: &'static str,
    path: &'static str,
    tag: &'static str,
    summary: &'static str,
) -> DocRoute {
    DocRoute {
        method,
        path,
        tag,
        summary,
        description: "",
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::Untyped("BUG: agents.rs route builder did not override the response"),
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        example: None,
    }
}

/// The ledger's own event body (`agents::ledger::decode_body`): one event
/// object or a JSON array of them. The third form, newline-delimited JSON, is
/// not a JSON value at all, so it is the operation's second media type
/// (`Req::JsonOrNdjson`) rather than a `"string"` in this schema's `type` —
/// which described a JSON-encoded string, a body `decode_body` would read as
/// one malformed line (review R2 #8).
fn run_events_body(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AgentRunEventsBody",
        schemars::json_schema!({
            "type": ["object", "array"],
            "items": { "type": "object" },
            "description": "One ledger event object ({row, ...} or {log: \"...\"} or \
                {done: true, ...}) or a JSON array of them. The same events may be sent as \
                application/x-ndjson instead, one object per line — ledger::decode_body reads \
                the raw body either way, whatever its Content-Type. A line or entry it cannot \
                parse is skipped and counted in the response's rejected list, never fails the \
                whole call."
        }),
    )
}

/// `run_close`'s hand-parsed body — no `Json<T>` extractor exists for it
/// (`web/api_agents.rs::run_close` reads `Bytes` and pulls fields off a bare
/// `Value` by hand), so this is written from what it actually reads rather
/// than derived.
fn run_close_body(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AgentRunCloseBody",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["done", "failed", "canceled"],
                    "description": "Absent defaults to \"done\"."
                },
                "detail": {"type": "string"},
                "output": {"description": "Any JSON value; carried into the stored result \
                    verbatim."}
            }
        }),
    )
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            description: "Every agent in the catalog, with its warnings, requirements and \
                last-run summary.",
            response: Resp::Json(|g| g.root_schema_for::<Vec<dto::AgentCard>>()),
            ..base("GET", "/api/agents", "agents", "List agents")
        },
        DocRoute {
            description: "Imports an agent from an exported manifest file (paste uses this \
                same route). replace overwrites an id that already exists; validate_only runs \
                the checks and reports without writing.",
            query: Some(|g| g.root_schema_for::<ImportQuery>()),
            request: Req::Raw("application/json"),
            response: Resp::Json(|g| g.root_schema_for::<dto::AgentImportReport>()),
            confirm_note: Some("replace overwrites an agent id that already exists here"),
            ..base("POST", "/api/agents/import", "agents", "Import an agent")
        },
        DocRoute {
            description: "One run with its rows: the executor's live buffer while it is in \
                flight, the stored result once it has ended. Ownership is checked before \
                existence — a foreign run and a missing one both answer the same refusal.",
            path_ints: &["job_id"],
            response: Resp::Json(|g| g.root_schema_for::<dto::AgentRunDetail>()),
            ..base(
                "GET",
                "/api/agents/runs/{job_id}",
                "agent-runtime",
                "Get one agent run",
            )
        },
        DocRoute {
            description: "Appends one ledger event, an array of them, or an NDJSON batch to \
                a run this caller owns — straight into the buffer the Run tab reads.",
            path_ints: &["job_id"],
            // Review R2 #8: NDJSON is raw text, not the JSON *string* the
            // old `"type": [.., "string"]` schema described.
            request: Req::JsonOrNdjson(run_events_body),
            response: Resp::Untyped("ad-hoc {ok, applied, rejected}; no DTO"),
            ..base(
                "POST",
                "/api/agents/runs/{job_id}/events",
                "agent-runtime",
                "Append agent run events",
            )
        },
        DocRoute {
            description: "Closes a run this caller owns with its terminal status.",
            path_ints: &["job_id"],
            request: Req::Json(run_close_body),
            response: Resp::Untyped("ad-hoc {ok}; no DTO"),
            ..base(
                "POST",
                "/api/agents/runs/{job_id}/close",
                "agent-runtime",
                "Close an agent run",
            )
        },
        DocRoute {
            description: "One agent's full definition — the owner's own document, or, for an \
                agent's own token, the same document with host paths substituted out.",
            response: Resp::Json(|g| g.root_schema_for::<dto::AgentDetail>()),
            ..base("GET", "/api/agents/{id}", "agent-runtime", "Get an agent")
        },
        DocRoute {
            description: "Downloads the agent as `<id>.agent.json`: its manifest plus an \
                envelope (export time, lmgw version, and — with include_config=1 — its \
                non-secret config values). No fixed shape this document models; a secret \
                never leaves.",
            query: Some(|g| g.root_schema_for::<ExportQuery>()),
            response: Resp::Untyped(
                "a JSON attachment: the manifest plus an export envelope, no fixed schema",
            ),
            ..base(
                "GET",
                "/api/agents/{id}/export",
                "agents",
                "Export an agent",
            )
        },
        DocRoute {
            description: "This agent's run history. For the agent's own token, a run's error \
                text has host paths substituted out the same way the detail read does.",
            response: Resp::Json(|g| g.root_schema_for::<Vec<dto::AgentRunSummary>>()),
            ..base(
                "GET",
                "/api/agents/{id}/runs",
                "agent-runtime",
                "List an agent's runs",
            )
        },
        DocRoute {
            description: "Opens a run from outside lmgw, exactly as the manifest's own \
                trigger would — \"one live run per agent\" still comes free from the \
                (kind, key) index; a second open while one is live is refused with the \
                running job's id.",
            request: Req::Json(|g| g.root_schema_for::<OpenBody>()),
            response: Resp::Untyped("ad-hoc {run, deadline_seconds}; no DTO"),
            ..base(
                "POST",
                "/api/agents/{id}/runs",
                "agent-runtime",
                "Open an agent run",
            )
        },
    ]
}
