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
    // -- Gateway -----------------------------------------------------------
    TagDef {
        id: "meta",
        name: "API description",
        group: Group::Gateway,
        description: "",
    },
    // -- Dashboard API ---------------------------------------------------
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
        id: "knowledge",
        name: "Knowledge bases",
        group: Group::DashboardApi,
        description: "Named collections of your own documents: create a base, upload \
                       files, follow the background job that reads and embeds them, view a \
                       file's text and search across bases. Uploading and changing a base's \
                       model or chunk size start jobs (kinds kb_ingest and kb_reembed, key \
                       kb:<id>) that answer at once; watch them on the jobs routes and the \
                       event stream.",
    },
    TagDef {
        id: "audio-lab",
        name: "Audio lab",
        group: Group::DashboardApi,
        description: "The dashboard's audio playground: the audio models and their voices, \
                       the library of reference clips a voice-cloning model reads (upload, \
                       preview, transcripts, delete), and the synthesis, transcription, \
                       alignment and generic-task routes that run a request through the same \
                       handlers as /v1/audio/* and /v1/tasks/*.",
    },
    TagDef {
        id: "image-lab",
        name: "Image lab",
        group: Group::DashboardApi,
        description: "The dashboard's image playground: the models that can draw, and the \
                       generate and edit routes that turn a form into a /v1/images/* request \
                       and run it through the same handlers.",
    },
    TagDef {
        id: "agents",
        name: "Agents",
        group: Group::DashboardApi,
        description: "",
    },
    // -- Device API ----------------------------------------------------------
    TagDef {
        id: "chat",
        name: "Chat",
        group: Group::DeviceApi,
        description: "The Chat API a client app uses with its device key (the `chat` \
                       capability): the threads and folders it follows, an ongoing \
                       conversation's current thread, and the change feed that keeps them \
                       current. Every route here refuses a device key with 401 \
                       device_key_unknown once it matches no device (it was rotated or \
                       deleted: pair the device again), 401 device_disabled while the device \
                       is disabled, and 401 key_expired past its expiry date. What is open \
                       when that happens ends: the feed with `revoked`, a turn's stream with \
                       an `error` event whose code is revoked. Both carry `kind`, what to do: \
                       device_disabled and key_expired wait for the gateway's side, \
                       key_unknown means pair the device again, and revoked is any other \
                       kind of key's revocation.",
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
        description: "The routes an agent container calls with its own token: reading its own \
                       definition and runs, and opening and driving a run.",
    },
];
