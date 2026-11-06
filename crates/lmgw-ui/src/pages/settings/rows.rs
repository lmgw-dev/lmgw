//! Settings — the rows of the table: common, audio, builds and the rest.

use super::*;

pub(super) const COMMON: &[Def] = &[
    // Network & access
    f("network", "listen", "bind_addr", "Bind address", Ctl::Mono)
        .req()
        .unit("restart to apply")
        .ph("127.0.0.1:8787")
        .terms("listen port host interface"),
    f(
        "network",
        "listen",
        "max_body_mb",
        "Max request body",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = unlimited")
    .terms("size upload payload limit"),
    f(
        "network",
        "access",
        "auth_enabled",
        "Require gateway API keys on /v1",
        Ctl::Bool,
    )
    .hint("off: any caller that reaches the bind address may use /v1")
    .terms("auth authentication bearer"),
    f(
        "network",
        "access",
        "self_admin",
        "Self-admin tools (lmgw__*)",
        Ctl::Choice(SELF_ADMIN),
    )
    .hint("The self-admin credential is managed on Usage → Keys.")
    .terms("mcp admin self-admin"),
    f(
        "network",
        "access",
        "api_keys",
        "API keys",
        Ctl::Link("/usage/keys"),
    )
    .terms(
        "api key keys token tokens credential credentials owner client bearer create revoke rotate",
    ),
    // Retention
    f(
        "retention",
        "logs",
        "retention_days",
        "Log retention",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .terms("request log traffic"),
    f(
        "retention",
        "logs",
        "retention_max_rows",
        "Log retention",
        Ctl::Int(0),
    )
    .s()
    .unit("max rows")
    .terms("request log traffic"),
    f(
        "retention",
        "jobs",
        "jobs_retention_days",
        "Job history",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .hint("0 = keep all"),
    f(
        "retention",
        "jobs",
        "jobs_max_rows",
        "Job history",
        Ctl::Int(0),
    )
    .s()
    .unit("max rows")
    .hint("0 = keep all"),
    f(
        "retention",
        "chat",
        "chat_archive_days",
        "Auto-archive idle chats",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .hint(
        "0 = never auto-archive; pinned threads and an ongoing conversation's current thread \
         are exempt; a folder's own retention overrides this",
    )
    .terms("chat thread conversation idle archive"),
    f(
        "retention",
        "chat",
        "chat_purge_days",
        "Delete archived chats",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .hint(
        "0 = keep archived threads forever; pinned threads are exempt; a folder's own \
         retention overrides this",
    )
    .terms("chat thread conversation purge delete"),
    f(
        "retention",
        "rollups",
        "usage_retention_months",
        "Rollup retention",
        Ctl::Int(0),
    )
    .s()
    .unit("months")
    .hint("0 = forever")
    .terms("usage hourly"),
    f(
        "retention",
        "responses",
        "responses_store",
        "Store responses",
        Ctl::Bool,
    )
    .hint("needed for previous_response_id and approvals")
    .terms("conversations"),
    f(
        "retention",
        "responses",
        "responses_retention_hours",
        "Evict after idle",
        Ctl::Int(0),
    )
    .s()
    .unit("hours")
    .hint("0 = never")
    .terms("conversations"),
    f(
        "retention",
        "responses",
        "responses_max_chains",
        "Keep at most",
        Ctl::Int(0),
    )
    .s()
    .unit("conversations")
    .hint("0 = unlimited"),
    // Usage & cost
    f("usage-cost", "money", "currency", "Currency", Ctl::Text)
        .s()
        .hint("a label, never a conversion")
        .ph("USD")
        .req(),
    f(
        "usage-cost",
        "money",
        "global_budget_micro",
        "Global budget",
        Ctl::Money,
    )
    .s()
    .unit("units")
    .hint("in that currency; empty = none")
    .ph("none")
    .terms("spend ceiling limit"),
    f(
        "usage-cost",
        "money",
        "global_budget_period",
        "Global budget resets",
        Ctl::Choice(PERIODS),
    )
    .s(),
    f(
        "usage-cost",
        "money",
        "quota_recheck_seconds",
        "Check an out-of-quota upstream again after",
        Ctl::Int(1),
    )
    .s()
    .unit("seconds")
    .hint("unless it said when (Retry-After); until then models with a fallback answer with it")
    .terms("insufficient_quota credit exhausted recheck"),
    f(
        "usage-cost",
        "compare",
        "local_reference_alias",
        "Compare local against",
        Ctl::Model(
            &["chat"],
            Some("the comparison is against a cloud price"),
            "none — the panel says no reference is configured",
        ),
    )
    .terms("counterfactual cloud reference alias"),
    // GPU
    // This computer (machine-check design §2.3): the check's card, and the
    // rows a search finds it by. The card is no field, so a search shows
    // these instead, each a link to it.
    f("gpu", "machine", "machine", "This computer", Ctl::Machine),
    f(
        "gpu",
        "machine",
        "machine.graphics_card",
        "Graphics card",
        Ctl::MachineLink,
    )
    .terms("gpu vram nvidia amd radeon geforce check ready local models"),
    f(
        "gpu",
        "machine",
        "machine.driver",
        "Graphics driver",
        Ctl::MachineLink,
    )
    .terms("nvidia driver akmod nouveau kernel module secure boot restart update"),
    f(
        "gpu",
        "machine",
        "machine.nvidia",
        "NVIDIA",
        Ctl::MachineLink,
    )
    .terms("cuda nvml geforce rtx nvidia-smi"),
    f(
        "gpu",
        "machine",
        "machine.toolkit",
        "Container toolkit",
        Ctl::MachineLink,
    )
    .terms("nvidia container toolkit nvidia-ctk podman repository dnf install set up"),
    f(
        "gpu",
        "machine",
        "machine.cdi",
        "Graphics card description (CDI)",
        Ctl::MachineLink,
    )
    .terms("cdi spec nvidia.yaml cdi-spec-dir device"),
    f(
        "gpu",
        "admission",
        "vram.enabled",
        labels::ARBITRATE_GPU_MEMORY,
        Ctl::Bool,
    )
    .hint("off = forward everything unchanged")
    .terms("vram admission"),
    f(
        "gpu",
        "admission",
        "vram.headroom_mb",
        "Headroom",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("kept free above the GGUF estimate")
    .terms("vram"),
    f(
        "gpu",
        "admission",
        "vram.budget_mb",
        labels::BUDGET,
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = the GPU's real total, as the driver reports it")
    .terms("vram"),
    f(
        "gpu",
        "admission",
        "vram.queue_timeout_seconds",
        labels::QUEUE_TIMEOUT,
        Ctl::Int(0),
    )
    .s()
    .unit("s")
    .hint("how long a request may wait; 0 = forever")
    .terms("vram"),
    f(
        "gpu",
        "admission",
        "vram.load_timeout_seconds",
        labels::LOAD_TIMEOUT,
        Ctl::Int(1),
    )
    .s()
    .unit("s")
    .hint("to wait for 'loaded'")
    .terms("vram"),
    f(
        "gpu",
        "admission",
        "vram.unload_timeout_seconds",
        labels::UNLOAD_TIMEOUT,
        Ctl::Int(1),
    )
    .s()
    .unit("s")
    .hint("to wait for 'unloaded'")
    .terms("vram"),
    f(
        "gpu",
        "admission",
        "vram.fallback_on_external",
        labels::FALLBACK_ON_EXTERNAL,
        Ctl::Bool,
    )
    .hint(
        "When a model does not fit because games, browsers or other apps use \
         GPU memory lmgw cannot free, its fallback answers at once instead of \
         queueing. Contention between lmgw's own models still queues. Turn \
         this off on shared-memory systems (APUs), where GPU memory is host \
         RAM that grows and shrinks with everything else running.",
    )
    .terms("external vram fallback apu shared memory"),
    f("gpu", "hold", "hold.active", "GPU hold", Ctl::Hold)
        .hint("Held models answer with their own fallback, where one is set (each model's editor).")
        .terms("pause gaming engage release gpu_hold fallback"),
    // Runtimes
    f(
        "runtimes",
        "containers",
        "container_prefix",
        "Container name prefix",
        Ctl::Mono,
    )
    .req()
    .ph("lmgw")
    .hint("takes effect on the next start of each model")
    .terms("podman name"),
];

pub(super) const AUDIO: &[Def] = &[
    f(
        "runtimes",
        "audio",
        "audio.image",
        labels::IMAGE,
        Ctl::Image(ImageClass::Audio),
    )
    .l()
    .hint("per-model overrides win")
    .terms("container podman build engines backends"),
    f(
        "runtimes",
        "audio",
        "audio.models_dir",
        labels::MODELS_DIR,
        Ctl::Mono,
    )
    .l()
    .unit("host")
    .terms("path"),
    f(
        "runtimes",
        "audio",
        "audio.voice_dir",
        "Voice library",
        Ctl::Mono,
    )
    .l()
    .unit("container path")
    .terms("voices wav"),
    f(
        "runtimes",
        "audio",
        "audio.voice_transcribe_alias",
        labels::CLIP_TRANSCRIPTS,
        Ctl::Model(
            &["asr"],
            None,
            "none — a clip is transcribed only when you ask",
        ),
    )
    .l()
    .hint(
        "writes an uploaded clip's transcript, which cloning models need; its configured \
         fallback is used as for any request, so pick a local model without one to keep your \
         clips on this machine",
    )
    .terms("voice clip transcript reference text asr transcribe library"),
    f(
        "runtimes",
        "audio",
        "audio.catalog_revision",
        labels::CATALOG_DOWNLOADS,
        Ctl::Choice(CATALOG_REVISIONS),
    )
    .l()
    .hint(
        "a spec's pin is the commit its engine was tested with; latest takes main even where a \
         spec pins one",
    )
    .terms("audio catalog download revision commit pin pinned latest main hugging face"),
    f(
        "runtimes",
        "audio",
        "audio.backend",
        labels::BACKEND,
        Ctl::Choice(BACKENDS),
    )
    .s(),
    f(
        "runtimes",
        "audio",
        "audio.device",
        "Device index",
        Ctl::Int(i64::MIN),
    )
    .s(),
    f(
        "runtimes",
        "audio",
        "audio.threads",
        "Threads",
        Ctl::Int(i64::MIN),
    )
    .s(),
    f(
        "runtimes",
        "audio",
        "audio.public_prefix",
        "Public prefix",
        Ctl::Mono,
    )
    .s(),
    f(
        "runtimes",
        "audio",
        "audio.request_timeout_seconds",
        "Request timeout",
        Ctl::Int(0),
    )
    .s()
    .unit("s")
    .hint("per call, once the model is up; 0 = none"),
    f(
        "runtimes",
        "audio",
        "audio.busy_timeout_ms",
        "Busy timeout",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("0 = wait forever"),
    f(
        "runtimes",
        "audio",
        "audio.idle_unload_ms",
        "Idle unload",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("0 = never"),
    f(
        "runtimes",
        "audio",
        "audio.min_free_memory_mb",
        "Min free memory",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = no guard"),
    f(
        "runtimes",
        "audio",
        "audio.max_request_body_mb",
        "Max request body",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = engine default"),
    f(
        "runtimes",
        "audio",
        "audio.lazy_load",
        "Lazy-load models",
        Ctl::Bool,
    ),
    f(
        "runtimes",
        "audio",
        "audio.extra_run_args",
        labels::EXTRA_RUN_ARGS,
        Ctl::Lines,
    )
    .unit("one per line")
    .terms("device gpu security-opt"),
    f(
        "runtimes",
        "audio",
        "apply.audio",
        "Apply to running containers",
        Ctl::Apply("audio"),
    )
    .terms("recreate restart"),
];

/// Container builds (container-builds §5, §7, §8).
pub(super) const BUILDS: &[Def] = &[
    f(
        "backends",
        "forge",
        "forge_tokens",
        labels::FORGE_TOKENS,
        Ctl::ForgeTokens,
    )
    .unit("write-only")
    .hint("per host; empty keeps what is stored")
    .terms("github gitlab gitea forgejo token credential pull request pr rate limit api backends"),
    f(
        "backends",
        "builds",
        "builds_dir",
        labels::BUILDS_DIR,
        Ctl::BuildsDir,
    )
    .l()
    .unit("host")
    .hint("git mirrors, per-run worktrees and logs; empty = the default")
    .terms("path mirror worktree cache tmpfs disk backends"),
    f(
        "backends",
        "builds",
        "build_update_check_hours",
        "Update check",
        Ctl::IntRange(0, MAX_BUILD_UPDATE_CHECK_HOURS as i64),
    )
    .s()
    .unit("hours")
    .hint("how often builds are checked for new commits; 0 to 8760 (a year), 0 = off")
    .terms("update badge poll interval commits moved range backends"),
    f(
        "backends",
        "builds",
        "backends_page",
        "Builds and images",
        Ctl::Link("/backends"),
    )
    .terms("engines backends builds images llama.cpp audio.cpp sd.cpp"),
];

pub(super) const REST: &[Def] = &[
    // Chat
    f(
        "chat",
        "prompt",
        "chat_system_prompt",
        "System prompt",
        Ctl::Prompt("/chat_system_prompt_builtin"),
    )
    .hint(
        "what a new chat thread starts with, as its own copy — existing threads keep theirs; \
         empty starts them without one",
    )
    .terms("chat default system prompt instructions persona new thread conversation"),
    f(
        "chat",
        "prompt",
        "chat_profile",
        "Profile for new threads",
        Ctl::Profile,
    )
    .hint(
        "the personality profile a new chat thread starts with, unless its folder names one; \
         Default is none. Edit profiles on the Chat profiles page",
    )
    .terms("chat personality profile persona concise voice new thread default"),
    f(
        "chat",
        "attachments",
        "chat_pdf_mode",
        "PDF attachments start as",
        Ctl::Choice(PDF_MODES),
    )
    .hint(
        "text works with every model; page images need a vision model; ask leaves the chip \
         unset and blocks Send until you choose",
    )
    .terms("chat pdf attachment text images pages ask mode"),
    f(
        "chat",
        "knowledge",
        "chat_kb_budget_tokens",
        "Excerpt budget per turn",
        Ctl::Int(1),
    )
    .s()
    .unit("tokens")
    .hint("knowledge-base excerpts one turn may carry; a thread can override it; above 0")
    .terms("chat knowledge base kb rag retrieval budget"),
    // The change feed client apps follow (client-apps design §2).
    f(
        "chat",
        "feed",
        "chat_feed_retention_days",
        "Keep changes for",
        Ctl::Int(0),
    )
    .s()
    .unit("days")
    .hint("a client away longer reloads what it shows (resync); 0 = keep all")
    .terms("chat feed change client app device retention resync cursor"),
    f(
        "chat",
        "feed",
        "chat_feed_keepalive_s",
        "Keep-alive every",
        Ctl::Int(1),
    )
    .s()
    .unit("seconds")
    .hint("told to each client, which takes a silent feed as dead after it; open feeds keep theirs; at most the longest keep-alive below")
    .terms("chat feed keepalive keep-alive ping dead link timeout"),
    f(
        "chat",
        "feed",
        "chat_feed_keepalive_max_s",
        "Longest keep-alive",
        Ctl::IntRange(1, lmgw_api_types::chat_feed::MAX_KEEPALIVE_S as i64),
    )
    .s()
    .unit("seconds")
    .hint("the most a feed may ask for itself, as a phone in the background does; bounds how long a feed whose device vanished stays open; at most 32 767 (about nine hours, the longest Linux accepts for a TCP keep-alive timer)")
    .terms("chat feed keepalive keep-alive maximum longest device phone background push"),
    f(
        "chat",
        "feed",
        "chat_feed_page_size",
        "Catch-up page",
        Ctl::IntRange(1, lmgw_api_types::chat_feed::MAX_PAGE_SIZE as i64),
    )
    .s()
    .unit("records")
    .hint("records read per query while a client catches up, each rendered only as it is sent; every record is still sent; at most 10 000")
    .terms("chat feed catch-up page size since"),
    f(
        "chat",
        "feed",
        "chat_feed_live_buffer",
        "Live buffer",
        Ctl::IntRange(1, lmgw_api_types::chat_feed::MAX_LIVE_BUFFER as i64),
    )
    .s()
    .unit("events")
    .hint("turns, voice and hold events held for a slow client; past it the client gets a fresh state; at most 65 536, as it is allocated whole")
    .terms("chat feed live buffer lag slow state turn voice hold"),
    // Agents & tools
    f(
        "agents",
        "agent-containers",
        "agent_script_image",
        "Agent script image",
        Ctl::Mono,
    )
    .l()
    .hint("empty restores the default")
    .ph("docker.io/library/node:24-alpine")
    .terms("script step node"),
    f(
        "agents",
        "agent-containers",
        "agent_origin_suffix",
        "Agent origin suffix",
        Ctl::Mono,
    )
    .req()
    .s()
    .ph("localhost")
    .terms("domain dns app ui cookie"),
    f(
        "agents",
        "tool-loop",
        "responses_max_tool_calls",
        "Max tool calls",
        Ctl::Int(0),
    )
    .s()
    .hint("per response")
    .terms("responses budget"),
    f(
        "agents",
        "tool-loop",
        "responses_timeout_seconds",
        "Time limit",
        Ctl::Int(1),
    )
    .s()
    .unit("s")
    .hint("per response")
    .terms("responses budget timeout"),
    f(
        "agents",
        "mcp",
        "sampling_alias",
        "MCP sampling alias",
        Ctl::Model(&["chat"], None, "none configured"),
    )
    .hint("answers sampling/createMessage for a server with no alias of its own"),
    // The device MCP host link (client-apps design §5.1).
    f(
        "agents",
        "mcp",
        "mcp.host_max_message_mb",
        "Device link: largest message",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = no bound of its own; a full-desktop screenshot is the large one")
    .terms("device host link websocket size"),
    f(
        "agents",
        "mcp",
        "mcp.host_max_frame_mb",
        "Device link: largest frame",
        Ctl::Int(0),
    )
    .s()
    .unit("MiB")
    .hint("0 = bounded by the message limit; not both 0")
    .terms("device host link websocket size"),
    f(
        "agents",
        "mcp",
        "mcp.host_ping_interval_s",
        "Device link: ping every",
        Ctl::Int(0),
    )
    .s()
    .unit("s")
    .hint("a missed pong closes the link; 0 = no pings")
    .terms("device host link liveness"),
    // MCP Tasks (MCP Tasks design §5.2): how often a job is asked about.
    f(
        "agents",
        "mcp",
        "mcp.task_poll_interval_s",
        "Task poll interval",
        Ctl::Int(1),
    )
    .s()
    .unit("s")
    .hint("used when a server suggests none; a server's status notifications act at once")
    .terms("mcp tasks job poll tasks/get status interval"),
    // Docs
    f(
        "docs",
        "ingest",
        "docs_ingest_reply_tokens",
        "Extraction reply budget",
        Ctl::Int(1),
    )
    .unit("tokens")
    .terms("quickdoc corpus"),
    f(
        "docs",
        "ingest",
        "docs_embed_batch",
        "Embed batch",
        Ctl::Int(1),
    )
    .s()
    .unit("texts")
    .hint("per embedding call")
    .terms("quickdoc embedding"),
    f(
        "docs",
        "ingest",
        "docs_fetch_delay_ms",
        "Fetch delay",
        Ctl::Int(0),
    )
    .s()
    .unit("ms")
    .hint("between fetches to one host; 0 = none")
    .terms("quickdoc crawl"),
    f(
        "docs",
        "search",
        "docs_rerank_model",
        "Rerank model",
        Ctl::Model(
            &["rerank"],
            None,
            "auto — the aux router's enabled rerank model",
        ),
    )
    .terms("quickdoc"),
    f(
        "docs",
        "search",
        "docs_search",
        "Search stage defaults",
        Ctl::Link("/docs/playground#params"),
    )
    .terms("quickdoc k_fts k_vec rrf playground stages"),
    // Tokens & updates
    f(
        "tokens",
        "tokens",
        "hf_token",
        "Hugging Face token",
        Ctl::Secret("/has_hf_token", "clear_hf_token"),
    )
    .l()
    .hint("empty keeps what is stored")
    .terms("huggingface hf download credential"),
    f(
        "tokens",
        "tokens",
        "update_token",
        "Update feed token",
        Ctl::Secret("/has_update_token", "clear_update_token"),
    )
    .l()
    .hint("only for a private feed; empty keeps what is stored")
    .terms("credential registry gitlab update feed"),
    f(
        "tokens",
        "updates",
        "update_check_enabled",
        "Check for new versions in the background",
        Ctl::Bool,
    )
    .terms("update version"),
    f(
        "tokens",
        "updates",
        "update_check_now",
        "Check for app update now",
        Ctl::CheckNow,
    )
    .terms("update version"),
    // Appearance
    f("appearance", "window", "theme", "Theme", Ctl::Theme)
        .s()
        .terms("dark light colour color"),
    f(
        "appearance",
        "window",
        "ui_scale",
        "Interface scale",
        Ctl::Scale,
    )
    .s()
    .terms("zoom size font"),
];
