//! `/api`'s general dashboard reads and writes — status, models, upstreams,
//! MCP servers & tools, downloads, settings, stored responses (api-docs
//! design §4.6). WP4.
//!
//! `GET /api/openapi.json` is documented here (not in a plane of its own):
//! WP6 left it for this plane (`server.rs`'s comment on the route, `openapi/
//! planes/inference.rs`'s sibling `GET /v1/openapi.json`).

use lmgw_api_types as dto;

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use crate::web::api::{
    ChainQuery, FlagsQuery, HfRepoQuery, IdQuery, InspectQuery, JobsQuery, LocalModelQuery,
    LogsQuery, ModelIdQuery, RungPlanQuery, SearchQuery,
};

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
        response: Resp::Untyped("BUG: dashboard.rs route builder did not override the response"),
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        example: None,
    }
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        // -- Meta ------------------------------------------------------------
        DocRoute {
            description: "The one /api read a page may do before it has a session: the \
                dashboard's footer and its login view print this.",
            response: Resp::Json(|g| g.root_schema_for::<dto::VersionInfo>()),
            ..base("GET", "/api/version", "meta", "Get the gateway version")
        },
        DocRoute {
            description: "This description, generated rather than hand-maintained.",
            response: Resp::Doc,
            ..base(
                "GET",
                "/api/openapi.json",
                "meta",
                "Get the full OpenAPI description",
            )
        },
        DocRoute {
            description: "Every reachable base URL, whether a key is required, the models a \
                client can already see, and the expose-all upstream prefixes — the connect \
                panel a fresh client reads to configure itself.",
            response: Resp::Json(|g| g.root_schema_for::<dto::ConnectInfo>()),
            ..base("GET", "/api/connect", "meta", "Get connection info")
        },
        // -- Status & live feed -----------------------------------------------
        DocRoute {
            description: "Gateway health in one call: uptime, live request stats, the \
                per-model container runtime, the GPU ledger, southbound MCP connections, and \
                config-object counts.",
            response: Resp::Json(|g| g.root_schema_for::<dto::GatewayStatus>()),
            ..base("GET", "/api/status", "status", "Get gateway status")
        },
        DocRoute {
            description: "A page of the request log, filterable by alias, upstream, key, \
                error kind and class — the same row shape the request SSE frame carries, so \
                the live feed and pagination share one type.",
            query: Some(|g| g.root_schema_for::<LogsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::LogsResponse>()),
            ..base("GET", "/api/logs", "status", "List recent requests")
        },
        DocRoute {
            description: "The live feed the dashboard opens once and keeps: stats, jobs, \
                runtime, vram, mcp, updates and chat frames on connect, then request/stats/mcp/\
                jobs/vram/runtime/updates frames as they change, a keys frame when a key's \
                state moves without a request (a device's connection opened or closed, a key \
                disabled, rotated or deleted), and a chat frame naming the threads, folders \
                and messages any writer changed in the Chat (a device, an owner key, this or \
                another dashboard window, the sweep): read them again. The first change of a \
                burst waits a fixed 200 ms for the rest, then one read names everything \
                changed up to it, so a stream sends at most one chat frame per 200 ms plus that \
                read (but for one its keep-alive read finds). \
                The chat frame on connect is a resync: read everything shown again, since \
                frames are not replayed across a reconnect. Never closes on its own.",
            response: Resp::Sse(&[
                ("stats", |g| g.root_schema_for::<dto::StatsView>()),
                ("jobs", |g| g.root_schema_for::<Vec<dto::JobRow>>()),
                ("runtime", |g| {
                    g.root_schema_for::<Vec<dto::RuntimeStatus>>()
                }),
                ("vram", |g| g.root_schema_for::<dto::VramStatus>()),
                ("mcp", |g| g.root_schema_for::<Vec<dto::McpStatus>>()),
                ("updates", |g| {
                    g.root_schema_for::<dto::builds::UpdatesSummary>()
                }),
                ("request", |g| g.root_schema_for::<dto::RequestRow>()),
                ("keys", |g| g.root_schema_for::<dto::KeysChanged>()),
                ("chat", |g| g.root_schema_for::<dto::ChatChanged>()),
            ]),
            ..base("GET", "/api/events", "status", "Open the live event stream")
        },
        DocRoute {
            description: "Poll half of the jobs feed; the SSE jobs frame on /api/events is \
                the push half.",
            query: Some(|g| g.root_schema_for::<JobsQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::JobsView>()),
            ..base("GET", "/api/jobs", "status", "List background jobs")
        },
        DocRoute {
            description: "The GPU ledger and the admission queue, freshly measured — the same \
                object the vram SSE frame carries, so a page can fetch once and then follow \
                the stream.",
            response: Resp::Json(|g| g.root_schema_for::<dto::VramStatus>()),
            ..base("GET", "/api/vram", "status", "Get the GPU ledger")
        },
        // -- Models -------------------------------------------------------------
        DocRoute {
            description: "Every configured model, all five kinds, plus candidate aliases and \
                expose-all passthrough upstreams — the Models page's one read.",
            response: Resp::Json(|g| g.root_schema_for::<dto::ModelsFull>()),
            ..base(
                "GET",
                "/api/models/full",
                "models",
                "List every configured model",
            )
        },
        DocRoute {
            description: "One local model's full definition, by id or model_id, chat or aux \
                class. target=image answers a different, UI-private shape this document does \
                not model.",
            query: Some(|g| g.root_schema_for::<LocalModelQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::LocalModelDetail>()),
            ..base(
                "GET",
                "/api/local-model",
                "models",
                "Get a local model's definition",
            )
        },
        DocRoute {
            description: "Checks a local model's config without starting it (missing files, a \
                port already in use, and the like) — the same read lmgw__local_model_check \
                answers.",
            query: Some(|g| g.root_schema_for::<ModelIdQuery>()),
            response: Resp::Untyped("no DTO; read by the lmgw__local_model_check self-admin tool"),
            ..base(
                "GET",
                "/api/local-model-check",
                "models",
                "Check a local model's config",
            )
        },
        DocRoute {
            description: "GGUF files under a class's models dir, matched against an optional \
                search string.",
            query: Some(|g| g.root_schema_for::<SearchQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::GgufFiles>()),
            ..base(
                "GET",
                "/api/gguf-files",
                "models",
                "List GGUF files on disk",
            )
        },
        DocRoute {
            description: "Reads a GGUF file's metadata, optionally probing it — the same read \
                lmgw__model_inspect answers.",
            query: Some(|g| g.root_schema_for::<InspectQuery>()),
            response: Resp::Untyped("no DTO; read by the lmgw__model_inspect self-admin tool"),
            ..base("GET", "/api/model-inspect", "models", "Inspect a GGUF file")
        },
        DocRoute {
            description: "A complete, ready-to-apply local-model parameter set derived from a \
                GGUF file's own metadata, with a rationale for each value — the editor's \
                wizard read, and the same one lmgw__local_model_plan answers.",
            query: Some(|g| g.root_schema_for::<InspectQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::PlanResult>()),
            ..base(
                "GET",
                "/api/local-model-plan",
                "models",
                "Plan a local model's parameters",
            )
        },
        DocRoute {
            description: "The ladder editor's per-rung footprint table: every rung shares \
                cache types, projector and drafter with the base row.",
            query: Some(|g| g.root_schema_for::<RungPlanQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::RungPlan>()),
            ..base(
                "GET",
                "/api/ladder-rung-plan",
                "models",
                "Plan a ladder's rungs",
            )
        },
        DocRoute {
            description: "The llama-server flags a model (or a search over models) would \
                start with — the same read lmgw__llama_flags answers.",
            query: Some(|g| g.root_schema_for::<FlagsQuery>()),
            response: Resp::Untyped("no DTO; read by the lmgw__llama_flags self-admin tool"),
            ..base(
                "GET",
                "/api/llama-flags",
                "models",
                "Preview a model's llama-server flags",
            )
        },
        DocRoute {
            description: "The signal path per model, composed server-side: which upstream, \
                which container, which fallback — shared verbatim with the Wiring page.",
            response: Resp::Json(|g| g.root_schema_for::<dto::WiringView>()),
            ..base(
                "GET",
                "/api/wiring",
                "models",
                "Get the model wiring diagram",
            )
        },
        // -- Upstreams ---------------------------------------------------------
        DocRoute {
            description: "Every configured upstream, secrets redacted to has_api_key and \
                extra header values shown as <set>.",
            response: Resp::Json(|g| g.root_schema_for::<dto::UpstreamsResponse>()),
            ..base("GET", "/api/upstreams", "upstreams", "List upstreams")
        },
        DocRoute {
            description: "One upstream's live model catalog, cached server-side for a short \
                TTL — a round trip to the upstream, so it is its own read rather than part of \
                the upstreams list.",
            query: Some(|g| g.root_schema_for::<IdQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::UpstreamModelsResponse>()),
            ..base(
                "GET",
                "/api/upstream-models",
                "upstreams",
                "List an upstream's live models",
            )
        },
        // -- MCP servers & tools -------------------------------------------------
        DocRoute {
            description: "Every registered southbound MCP server, its connection state and \
                its exposed tools.",
            tool: Some("lmgw__mcp_servers"),
            response: Resp::Json(|g| g.root_schema_for::<dto::McpServersResponse>()),
            ..base(
                "GET",
                "/api/mcp-servers",
                "tools",
                "List registered MCP servers",
            )
        },
        DocRoute {
            description: "One server's exposed tools — what a Chat thread's MCP picker \
                offers.",
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<dto::McpServerToolsResponse>()),
            ..base(
                "GET",
                "/api/mcp-servers/{id}/tools",
                "tools",
                "List one MCP server's tools",
            )
        },
        DocRoute {
            description: "The whole northbound tool surface: the built-in toolsets and every \
                registered server's tools, each with its source and why it is or is not \
                offered.",
            response: Resp::Json(|g| g.root_schema_for::<dto::ToolInventory>()),
            ..base("GET", "/api/tools", "tools", "List the tool inventory")
        },
        // -- Downloads -----------------------------------------------------------
        DocRoute {
            description: "A Hugging Face repo's files, classified into weights, mmproj \
                projector and speculative drafter candidates.",
            query: Some(|g| g.root_schema_for::<HfRepoQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::RepoFiles>()),
            ..base(
                "GET",
                "/api/hf/repo",
                "downloads",
                "List a Hugging Face repo's files",
            )
        },
        DocRoute {
            description: "The download queue and history.",
            response: Resp::Json(|g| g.root_schema_for::<dto::DownloadsView>()),
            ..base("GET", "/api/hf/downloads", "downloads", "List downloads")
        },
        DocRoute {
            description: "audio.cpp's own spec catalog, from the cached snapshot only — an \
                empty fetched_at means \"never fetched here\"; a page load never waits on \
                GitHub.",
            response: Resp::Json(|g| g.root_schema_for::<dto::AudioCatalog>()),
            ..base(
                "GET",
                "/api/audio/catalog",
                "downloads",
                "Get the audio.cpp spec catalog",
            )
        },
        // -- Settings --------------------------------------------------------------
        DocRoute {
            description: "Every dashboard-editable setting: bind address, self-admin mode + \
                token, secrets (masked), and all three class definitions. Deliberately \
                broader than what the settings_set self-admin tool exposes.",
            response: Resp::Json(|g| g.root_schema_for::<dto::SettingsFull>()),
            ..base("GET", "/api/settings-full", "settings", "Get every setting")
        },
        // -- Stored responses --------------------------------------------------------
        DocRoute {
            description: "Every stored /v1/responses conversation chain, most recent first, \
                capped at 200.",
            response: Resp::Json(|g| g.root_schema_for::<dto::ResponsesIndex>()),
            ..base(
                "GET",
                "/api/responses",
                "responses",
                "List stored response chains",
            )
        },
        DocRoute {
            description: "One conversation chain's stored responses, each with its outline, \
                text, pending tool calls and full body.",
            query: Some(|g| g.root_schema_for::<ChainQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<dto::ChainDetail>()),
            ..base(
                "GET",
                "/api/responses/chain",
                "responses",
                "Get a response chain",
            )
        },
    ]
}
