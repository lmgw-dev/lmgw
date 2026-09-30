//! The MCP face (`run.provides.mcp: "/mcp"`): two read-only tools over the
//! owner's folder, `search` and `read`. lmgw lists them on its aggregate
//! `/mcp` as `folder-chat__search` and `folder-chat__read`, reached through
//! its Admin-gated `/agents/folder-chat/mcp` proxy.
//!
//! **rmcp's own server** (`server` + `transport-streamable-http-server`, the
//! same rmcp 2.0.0 whose client lmgw speaks with), in its **stateless JSON
//! mode**: every `POST /mcp` is answered on its own with `application/json`,
//! no `Mcp-Session-Id`, no server-to-client SSE stream. Both tools are plain
//! request/response and the server never needs to push anything, so a session
//! would only be state to lose on the next idle-stop; and with no long-lived
//! stream open, a shutdown has nothing to wait for. lmgw's client accepts a
//! server that hands out no session (rmcp's `allow_stateless`, on by default).
//!
//! rmcp's own DNS-rebinding check (a loopback `Host`) stays on: the proxy
//! reaches the container on its published loopback port, so the `Host` it
//! sends is `127.0.0.1:<port>`. The guards in [`crate::server`] run first.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::chat::{default_search_params, AskError, MAX_EXCERPTS};
use crate::gateway::GatewayError;
use crate::FolderChat;

/// `search`'s `k` when the caller gives none: the same number of excerpts the
/// chat itself is offered ([`MAX_EXCERPTS`]).
pub const DEFAULT_SEARCH_K: usize = MAX_EXCERPTS;

/// The `search` tool's description, with the retrieval depths it runs with
/// (quickdoc's `SearchParams`, as the chat uses them) written in.
pub fn search_description() -> String {
    let d = default_search_params(true);
    format!(
        "Search the owner's folder (the one bound to the folder-chat agent: their notes, \
         documents, source code and PDFs) for the excerpts that best match a query. Use it first \
         whenever a question might be answered by those files, and to find where something is \
         before reading it. Returns up to k excerpts, best first, each with: path (relative to \
         the folder; pass it to `read` unchanged), heading_path (the markdown headings above the \
         excerpt, or 'page N' for a PDF), page (PDFs only), start_line and end_line (1-based, \
         inclusive, the lines of the file as it was indexed at the last sync, in the same text \
         `read` returns; if the file was edited since, they may be off until the next sync; null \
         only when the index holds no lines for the excerpt), score (higher is better; \
         comparable only within one result) and text (the excerpt verbatim). Matching is \
         hybrid, by meaning (embeddings) and by exact words (BM25), so both a natural-language \
         question and the literal terms you expect in the text work: the k_fts ({k_fts}) best \
         BM25 matches and the k_vec ({k_vec}) nearest vectors — both raised to k when k is \
         larger — are fused by reciprocal rank (rrf_k {rrf_k}). When the owner configured a \
         rerank model, the first k_rerank ({k_rerank}) of the fused list are reordered by it; \
         with k above k_rerank, the results after the first k_rerank are beyond the reranked \
         set, in fused order, and their scores are fusion scores, not rerank scores. The result \
         names the reranker (reranked_by), why there was none (rerank_skipped), and the depths \
         used (retrieval, notes). It searches the index as of the last sync. To see more context \
         around a hit, call `read` with its path and a line range around start_line..end_line.",
        k_fts = d.k_fts,
        k_vec = d.k_vec,
        rrf_k = d.rrf_k,
        k_rerank = d.k_rerank,
    )
}

const READ_DESCRIPTION: &str = "Read one file of the owner's folder by the path `search` \
returned (relative to the folder). Returns the file's text as the index sees it: the file itself \
for markdown, plain text and source code; for a PDF, the text pdftotext extracts, pages \
separated by form-feed characters, followed by any page a vision model read from its image, each \
under a line '[page N, read by <model> from the page image]' — that part is the model's reading, \
not the file's own text, and its names and numbers can be misread. Lines count from 1. Give start_line and end_line (both \
inclusive) to read part of a file; with only start_line it reads to the end, with only end_line \
from the first line. With neither, the WHOLE file is returned, however large it is: there is no \
size cap, so for anything longer than a page ask for a range, for example 30 lines either side \
of a search hit. An end_line past the last line ends at the last line and says so. Refused, \
each with a message naming the rule: absolute paths, '..' components, hidden files or \
directories (a name starting with '.'), symlinks anywhere on the path, files the folder's \
.gitignore or .ignore excludes, and types other than markdown, plain text, source code and PDF.";

const INSTRUCTIONS: &str = "Search and read the files of one folder the owner bound to the \
folder-chat agent. Call `search` first; `read` takes the paths it returns, with a line range \
around a hit.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    k: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

fn schema(v: Value) -> Arc<JsonObject> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => Arc::new(JsonObject::new()),
    }
}

/// The two tools, as `tools/list` returns them.
pub fn tools() -> Vec<Tool> {
    let ro = || {
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false)
    };
    vec![
        Tool::new(
            "search",
            search_description(),
            schema(json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to look for: a question in plain words, or the terms you expect in the text."
                    },
                    "k": {
                        "type": "integer",
                        "minimum": 1,
                        "description": format!("How many excerpts to return, best first. Default {DEFAULT_SEARCH_K}. Fewer come back only when the folder holds fewer.")
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            })),
        )
        .with_title("Search the folder")
        .with_annotations(ro()),
        Tool::new(
            "read",
            READ_DESCRIPTION,
            schema(json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file's path relative to the folder, exactly as search returns it, e.g. 'notes/setup.md'."
                    },
                    "start_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "First line to return (1-based, inclusive). Omit to start at line 1."
                    },
                    "end_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Last line to return (inclusive). Omit to read to the end."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            })),
        )
        .with_title("Read a file of the folder")
        .with_annotations(ro()),
    ]
}

/// The handler rmcp calls: one per request in stateless mode, cheap to make.
#[derive(Clone)]
pub struct McpFace {
    app: Arc<FolderChat>,
}

impl McpFace {
    pub fn new(app: Arc<FolderChat>) -> Self {
        Self { app }
    }

    async fn search(&self, args: SearchArgs) -> CallToolResult {
        let k = args.k.unwrap_or(DEFAULT_SEARCH_K);
        if k == 0 {
            return tool_error("k must be at least 1");
        }
        match self.app.search(&args.query, k).await {
            Ok(r) => {
                let hits: Vec<Value> = r
                    .hits
                    .iter()
                    .map(|c| {
                        json!({
                            "rank": c.n,
                            "path": c.path,
                            "heading_path": c.heading_path,
                            "page": c.page,
                            "start_line": c.start_line,
                            "end_line": c.end_line,
                            "score": c.score,
                            "text": c.text,
                        })
                    })
                    .collect();
                CallToolResult::structured(json!({
                    "query": args.query,
                    "k": k,
                    "hits": hits,
                    "reranked_by": r.rerank_model,
                    "rerank_skipped": r.rerank_skipped,
                    "retrieval": r.retrieval,
                    "notes": r.notes,
                }))
            }
            Err(e) => tool_error(search_error(&e)),
        }
    }

    async fn read(&self, args: ReadArgs) -> CallToolResult {
        let text = match self.app.read_file(&args.path).await {
            Ok(t) => t,
            Err(e) => return tool_error(e.to_string()),
        };
        let slice = match text.lines(args.start_line, args.end_line) {
            Ok(s) => s,
            Err(e) => return tool_error(e.to_string()),
        };
        let whole = args.start_line.is_none() && args.end_line.is_none();
        let mut header = if slice.total_lines == 0 {
            format!("{}: the file is empty (0 lines).", slice.path)
        } else if whole {
            format!("{}: all {} lines.", slice.path, slice.total_lines)
        } else {
            format!(
                "{}: lines {}-{} of {}.",
                slice.path, slice.start_line, slice.end_line, slice.total_lines
            )
        };
        if let Some(n) = &slice.note {
            header.push(' ');
            header.push_str(n);
            header.push('.');
        }
        CallToolResult::success(vec![ContentBlock::text(format!(
            "{header}\n\n{}",
            slice.text
        ))])
    }
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn search_error(e: &AskError) -> String {
    match e {
        AskError::NoIndex => "there is no index yet: the folder has not finished its first sync \
                              (or the last one stopped). Try again once the sync is done."
            .into(),
        AskError::EmptyQuestion => "query is empty; say what to look for".into(),
        AskError::IndexModelChanged { .. } => {
            format!("{e} Search works again once the owner's next sync has finished.")
        }
        AskError::Gateway(g @ GatewayError::GpuHold { .. }) => format!(
            "{g}. The owner paused local models on purpose, so the query cannot be embedded \
             right now; do not retry until the hold is released."
        ),
        other => other.to_string(),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(tool: &str, args: Option<JsonObject>) -> Result<T, String> {
    serde_json::from_value(Value::Object(args.unwrap_or_default()))
        .map_err(|e| format!("invalid arguments for {tool}: {e}"))
}

impl ServerHandler for McpFace {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "folder-chat",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|t| t.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "search" => Ok(match parse::<SearchArgs>("search", request.arguments) {
                Ok(a) => self.search(a).await,
                Err(e) => tool_error(e),
            }),
            "read" => Ok(match parse::<ReadArgs>("read", request.arguments) {
                Ok(a) => self.read(a).await,
                Err(e) => tool_error(e),
            }),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool '{other}'; this server has 'search' and 'read'"),
                None,
            )),
        }
    }
}

/// The tower service mounted at `/mcp`. `shutdown` ends any request still
/// waiting on a tool when the server stops.
pub fn service(
    app: Arc<FolderChat>,
    shutdown: CancellationToken,
) -> StreamableHttpService<McpFace, NeverSessionManager> {
    let face = McpFace::new(app);
    StreamableHttpService::new(
        move || Ok(face.clone()),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true)
            .with_cancellation_token(shutdown),
    )
}
