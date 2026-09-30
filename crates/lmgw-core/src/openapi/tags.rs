//! The tag list (api-docs design §4.3), final as of WP1: every `DocRoute` and
//! `OpDoc` names one of these by `id`, and the rail groups by [`Group`] then
//! lists tags in this order (§6.4).
//!
//! `name` is the human label the rail head and the OpenAPI tag's own
//! `description` show; `description` is a longer sentence for the handful of
//! tags that need one — empty means "use `name`" (`build.rs`'s
//! `tag_description`). `id` is what a `DocRoute`/`OpDoc` references and what
//! the emitted tag's `name` field is (OpenAPI's `tags: […]` on an operation
//! matches by that field, not by this list's own `name`).

use super::registry::Group;

pub(crate) struct TagDef {
    pub id: &'static str,
    pub name: &'static str,
    pub group: Group,
    pub description: &'static str,
}

/// Group order, then declaration order — the rail's own order (§6.4).
pub(crate) const TAGS: &[TagDef] = &[
    // -- Inference -----------------------------------------------------
    TagDef {
        id: "openai",
        name: "OpenAI-compatible",
        group: Group::Inference,
        description: "",
    },
    TagDef {
        id: "anthropic",
        name: "Anthropic-compatible",
        group: Group::Inference,
        description: "",
    },
    TagDef {
        id: "llamacpp",
        name: "llama.cpp-compatible",
        group: Group::Inference,
        description: "",
    },
    TagDef {
        id: "lmgw-inference",
        name: "lmgw extensions",
        group: Group::Inference,
        description: "",
    },
    TagDef {
        id: "mcp",
        name: "MCP",
        group: Group::Inference,
        description: "",
    },
    // -- Dashboard API ---------------------------------------------------
    TagDef {
        id: "meta",
        name: "API description",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "session",
        name: "Session",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "status",
        name: "Status & live feed",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "models",
        name: "Models",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "upstreams",
        name: "Upstreams",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "tools",
        name: "MCP servers & tools",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "downloads",
        name: "Downloads",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "settings",
        name: "Settings",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "responses",
        name: "Stored responses",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "usage",
        name: "Usage",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "docs",
        name: "Doc corpora",
        group: Group::DashboardApi,
        description: "",
    },
    TagDef {
        id: "agents",
        name: "Agents",
        group: Group::DashboardApi,
        description: "",
    },
    // -- Ops ---------------------------------------------------------------
    // No human names were carried over from the design table (§4.3 lists ids
    // only); these are the smallest reasonable reading of the self-admin
    // catalog's own grouping (`mcp/selfadmin/catalog/{routing,models,runtime,
    // sources,prices,docs,builds,agents,reads}.rs`).
    TagDef {
        id: "ops-routing",
        name: "Routing ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-models",
        name: "Model ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-runtime",
        name: "Runtime ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-downloads",
        name: "Download ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-settings",
        name: "Settings ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-tools",
        name: "Tool ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-keys",
        name: "Key ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-usage",
        name: "Usage & pricing ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-responses",
        name: "Stored-response ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-builds",
        name: "Backends & build ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-bench",
        name: "Benchmark ops",
        group: Group::Ops,
        description: "",
    },
    TagDef {
        id: "ops-agents",
        name: "Agent ops",
        group: Group::Ops,
        description: "",
    },
    // -- Agent runtime -------------------------------------------------------
    TagDef {
        id: "agent-runtime",
        name: "Agent runtime",
        group: Group::AgentRuntime,
        description: "Every AgentSelf and Ledger route — an agent container's own view of \
                       itself and its runs.",
    },
];
