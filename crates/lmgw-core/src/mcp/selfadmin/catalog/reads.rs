//! The read-only tools (`lmgw__status` … `lmgw__forge_prs`): every
//! `Builtin` with `writes: false`, gated on
//! [`SelfAdmin::allows_read`](crate::config::SelfAdmin::allows_read)
//! alone. Order is the `tools/list` contract — do not resort.

use crate::mcp::selfadmin::{bool_p, enum_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        // ---------------- reads ----------------
        Builtin {
            name: "lmgw__status",
            writes: false,
            description:
                "Health of the lmgw gateway itself: version, uptime, request and error \
                 counters, the per-model container runtime (one podman container per loaded \
                 chat/aux/audio model — its state, port, uptime and in-flight requests), the \
                 GPU ledger those containers share (measured free memory, which models are \
                 resident and what they are estimated to cost, and any requests queued waiting \
                 for room), every southbound MCP server's connection status, and counts of \
                 configured objects. A ladder model's runtime entry additionally carries rung \
                 k/n (which weights file is loaded now, out of how many configured) and, while \
                 it is moving to a higher one, a climbing state naming the target and the \
                 reason ('prompt 41,210 + 8,192 > 30,000'). A runtime entry a background \
                 candidate alias started carries owner: 'background' until an ordinary request \
                 claims it, and draining_for_owner while an owner admission is waiting on it \
                 (candidate-aliases design §4.4-§4.5); both are absent on every other entry. \
                 When any candidate alias is configured, a candidate_aliases array lists each \
                 one's name, mode (background/owner), primary, alternates, enabled facets, \
                 problems and deferrals_24h — how many times it answered gpu_deferred (a \
                 background request declined rather than disturb the owner) in the last 24 \
                 hours; absent entirely on an install with none configured. Start here when \
                 asked how the gateway is doing — including 'why is my request slow', 'what is \
                 on the GPU' and 'is my background job actually running or stuck deferring'.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__models",
            writes: false,
            description:
                "List the model names this gateway can route, with their source. Use this \
                 to find the exact alias to send to /v1/chat/completions or /v1/messages. \
                 Entries carry the same capabilities/max_output_tokens/notes object that GET \
                 /v1/models publishes; absent fields are unknown. kind='alias' (and 'all') \
                 also lists candidate aliases (candidate-aliases design) — 'kind': \
                 'candidate_alias' distinguishes them from a plain alias — each with its \
                 candidates list (primary first), background flag, enabled facets, which \
                 candidates are routable right now, any problems (a candidate or the fallback \
                 that stopped supporting an enabled facet, say), and deferrals_24h — how many \
                 times a background alias answered gpu_deferred (declined rather than disturb \
                 the owner) in the last 24 hours. Answering this makes the same cached catalog \
                 HTTP calls to expose-all upstreams that /v1/models makes (warm calls are free; \
                 a cold one fetches each reachable upstream's catalog once).",
            props: vec![
                (
                    "kind",
                    enum_p(
                        "Which model source to list. Default 'all'.",
                        &["all", "alias", "local", "aux", "audio", "image"],
                    ),
                ),
                (
                    "search",
                    str_p("Case-insensitive substring filter on the model name."),
                ),
            ],
            required: &[],
        },
        Builtin {
            name: "lmgw__upstreams",
            writes: false,
            description:
                "List configured upstream providers (OpenAI-compatible, Anthropic, Gemini, \
                 local llama.cpp) with their base URLs, protocols and enabled state. API \
                 keys are never returned, only whether one is set.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__mcp_servers",
            writes: false,
            description:
                "List the MCP servers lmgw aggregates southbound, each with its transport, \
                 endpoint, live connection status and discovered tool count. Secrets in env \
                 and headers are redacted.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__logs",
            writes: false,
            description:
                "Tail the gateway request log (every /v1 request and MCP tool call, newest \
                 first) with status, latency, tokens and error details. fallback_reason says \
                 why a fallback answered instead of the requested local model (hold, \
                 external_vram, background, unavailable); upstream is the fallback. rung is the 1-based rung \
                 a ladder model's row was judged on (empty for a row without a ladder, and for \
                 one a fallback answered instead). Use it to diagnose a failing model or \
                 upstream.",
            props: vec![
                ("limit", int_p("How many rows to return. Default 50.")),
                (
                    "errors_only",
                    bool_p("Only rows with an HTTP status >= 400. Default false."),
                ),
                (
                    "alias",
                    str_p("Only rows for this requested model alias / tool name."),
                ),
                (
                    "before_id",
                    int_p("Only rows older than this log id, for paging."),
                ),
            ],
            required: &[],
        },
        Builtin {
            name: "lmgw__settings",
            writes: false,
            description:
                "Read gateway settings: bind address, auth, log retention, the self-admin \
                 mode, the four container configurations (chat, aux, audio, image), the GPU \
                 hold (active, fallback_alias), the external-VRAM fallback switch \
                 (vram.fallback_on_external) and the container-build settings (builds \
                 directory, update-check interval, which hosts have a forge token). Tokens \
                 are redacted.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__local_model_get",
            writes: false,
            description:
                "The complete stored configuration of one local model — chat, aux \
                 (embedding / rerank) or image (stable-diffusion.cpp) — every parameter, \
                 whether each file it references actually exists on disk, static problems, \
                 and the exact command line its container is started with. Use this to read \
                 back what you just wrote; lmgw__models only lists names. A model id is \
                 looked up in the chat table first, then aux, then image; the result says \
                 which ('class'). An image row comes back with its files/args maps, its \
                 modes and edit flag, and a files_present map saying which paths resolve. A \
                 chat row with a ladder additionally carries 'rungs': every rung (the base \
                 included, as rung 1) with its own command line, its per-slot context (ctx_size \
                 / parallel) and its switchover (the largest prompt that rung takes with the \
                 full max output before a bigger one makes the model climb) — null when the row \
                 is not a ladder.",
            props: vec![
                ("id", int_p("Row id (per class — pass target with it for a non-chat row).")),
                ("model_id", str_p("Model id, if you don't have the row id.")),
                (
                    "target",
                    enum_p(
                        "Which class to look in. Omit to search chat, then aux, then image.",
                        &["chat", "aux", "image"],
                    ),
                ),
            ],
            required: &[],
        },
        Builtin {
            name: "lmgw__local_model_check",
            writes: false,
            description:
                "Static health of every configured local model — chat, aux and image \
                 alike: missing GGUFs, a projector or drafter passed as weights, \
                 speculative-decoding settings the model cannot support, for embedding / \
                 rerank models a kind or pooling that contradicts the GGUF header, and for \
                 image models a files path that is not on disk, a files/args key that is not \
                 an sd-server flag, or a row naming neither of the two ways to load a \
                 pipeline. Answers \"which of my models are broken?\" without mutating \
                 anything. Models with nothing wrong are omitted unless you name one. A \
                 clean result does not prove a model loads — only lmgw__local_model_test \
                 does that.",
            props: vec![
                (
                    "model_id",
                    str_p("Check just this model. Omit to check them all."),
                ),
                (
                    "target",
                    enum_p(
                        "Restrict to one class. Omit to check chat, aux and image.",
                        &["chat", "aux", "image"],
                    ),
                ),
            ],
            required: &[],
        },
        Builtin {
            name: "lmgw__gguf_files",
            writes: false,
            description:
                "List the model files present in one class's models directory (chat by \
                 default; target=aux for the embedding / rerank dir), with sizes and whether \
                 a model entry already uses each one. This is how you discover what is on \
                 disk without shell access — the paths returned are exactly what gguf_path, \
                 mmproj_path and draft_gguf_path expect, together with the same target. \
                 target=image returns EVERY file kind sd-server loads — .gguf AND \
                 .safetensors, .ckpt, .pt, .pth — not only GGUF, because a pipeline's VAE \
                 and text encoders are .safetensors; the paths are what an image row's \
                 'files' values take, and 'role_guess' names the likely files key \
                 (diffusion, checkpoint, vae, text_encoder, lora, upscaler) as a filename \
                 heuristic. (The tool keeps its name: renaming one every agent already knows \
                 is worse than a name that is one word too narrow.)",
            props: vec![
                (
                    "search",
                    str_p("Case-insensitive substring filter on the path."),
                ),
                (
                    "target",
                    enum_p(
                        "Which class's models directory the path is under. Default 'chat'. \
                         'aux' is the embedding / rerank models dir, 'image' the \
                         stable-diffusion.cpp one — the four classes keep separate trees, so \
                         a file downloaded with lmgw__hf_add target=aux is only visible here \
                         with target=aux.",
                        &["chat", "aux", "audio", "image"],
                    ),
                ),
            ],
            required: &[],
        },
        Builtin {
            name: "lmgw__model_inspect",
            writes: false,
            description:
                "Read a GGUF's own metadata: architecture, trained context length, layer and \
                 head counts, quantization, whether it carries a chat template, whether it \
                 has MTP layers, and whether it is weights / a multimodal projector / a \
                 speculative drafter. Also says which class serves it ('serve_as': an \
                 embedding or rerank model is an aux model, recognised by the pooling_type \
                 or classifier head in its header, whatever directory it sits in), \
                 estimates KV cache cost, and optionally probes whether the RUNNING \
                 llama.cpp build supports the architecture at all — a build older than the \
                 model fails to load with 'unknown model architecture', and this is the \
                 cheap way to find that out. Reads only the file header.",
            props: vec![
                (
                    "gguf_path",
                    str_p("GGUF path relative to the target's models directory."),
                ),
                (
                    "target",
                    enum_p(
                        "Which class's models directory the path is under. Default 'chat'. \
                         'aux' is the embedding / rerank models dir — the four classes keep \
                         separate trees, so a file downloaded with lmgw__hf_add target=aux is \
                         only visible here with target=aux. 'image' is the \
                         stable-diffusion.cpp tree, whose files are mostly not GGUF: only a \
                         .gguf there has a header to read.",
                        &["chat", "aux", "audio", "image"],
                    ),
                ),
                (
                    "probe",
                    bool_p(
                        "Ask the running llama-server whether it knows this architecture. \
                         An unsupported one is reported in about a second; a supported one \
                         costs the full ~10s budget, because the check passing means the \
                         model went on loading. Default true.",
                    ),
                ),
            ],
            required: &["gguf_path"],
        },
        Builtin {
            name: "lmgw__local_model_plan",
            writes: false,
            description:
                "Derive a complete, ready-to-apply parameter set for a GGUF that is already \
                 in a models directory, by reading its metadata and finding its companion \
                 files (projector, drafter) beside it. The result's 'class' says which set \
                 tool takes it: a chat model's params are lmgw__local_model_set's; an \
                 embedding or rerank model (recognised from the header, not the filename) \
                 gets kind, pooling and ctx_size for lmgw__aux_model_set instead — pass \
                 target=aux for a file in the aux models dir. Every value carries a \
                 rationale, and warnings name combinations that would fail to load. Start \
                 here instead of guessing llama.cpp parameters. If a configured model \
                 already uses this GGUF, 'configured_as' additionally reports how each one \
                 differs from the plan and carries a ready-made update patch — so this is \
                 also the repair tool, not just the add-a-model tool. For target=image the \
                 result's params are lmgw__image_model_set's, matched from the shipped \
                 recipe list rather than read out of the file.",
            props: vec![
                (
                    "gguf_path",
                    str_p("Weights GGUF path relative to the target's models directory."),
                ),
                (
                    "target",
                    enum_p(
                        "Which class's models directory the path is under. Default 'chat'. \
                         'aux' is the embedding / rerank models dir — the classes keep \
                         separate trees, so a file downloaded with lmgw__hf_add target=aux is \
                         only visible here with target=aux. 'image' plans from the shipped \
                         RECIPE list rather than from a header — pass a diffusion model or \
                         an all-in-one checkpoint under the image models dir and the matching \
                         family's whole parameter set comes back, with any component that is \
                         not on disk named together with the lmgw__hf_add call that fetches \
                         it; an unrecognised file says so and lists the recipes it knows.",
                        &["chat", "aux", "image"],
                    ),
                ),
                (
                    "probe",
                    bool_p(
                        "Also check whether the running llama.cpp build supports the \
                         architecture. Adds up to ~10s. Default true; set false when you \
                         only want the parameters.",
                    ),
                ),
            ],
            required: &["gguf_path"],
        },
        Builtin {
            name: "lmgw__llama_flags",
            writes: false,
            description:
                "The llama-server flag vocabulary of a container IMAGE: every supported \
                 long flag, the allowed values for the flags that take a fixed set, and \
                 which flags that build has removed. Use it when a parameter is rejected, \
                 or to check whether a build supports a feature at all — the vocabulary is \
                 a property of the image, not of lmgw, and models can run different \
                 images. Read from a throwaway container, so nothing has to be running.",
            props: vec![
                (
                    "search",
                    str_p("Case-insensitive substring filter on the flag name."),
                ),
                (
                    "model",
                    str_p(
                        "Whose vocabulary to read: a configured model id (any class) uses \
                         the image that model runs; anything else is taken as an image \
                         reference. Default: the chat class image.",
                    ),
                ),
            ],
            required: &[],
        },
        Builtin {
            name: "lmgw__hf_repo",
            writes: false,
            description:
                "List the weights files in a Hugging Face repo, each classified by role — \
                 'weights', 'mmproj' (multimodal projector) or 'drafter' (speculative \
                 decoding) — with its quantization label and size. Split GGUFs are collapsed \
                 into one entry. Call this before lmgw__hf_add to choose a quantization. \
                 With target=image it lists .safetensors / .ckpt / .pt / .pth as well as \
                 .gguf, and names the roles of an image pipeline instead (diffusion, \
                 checkpoint, vae, text_encoder, lora, upscaler) — but one image pipeline \
                 spans several repos, so prefer lmgw__image_recipes, which names every \
                 component of a known family with its repo, file and size.",
            props: vec![
                (
                    "repo",
                    str_p(
                        "Repo id like 'unsloth/Qwen3.5-9B-GGUF' — owner/name only, not a \
                         full huggingface.co URL.",
                    ),
                ),
                ("search", str_p("Case-insensitive substring filter on the filename.")),
                (
                    "target",
                    enum_p(
                        "Which class's file kinds and role vocabulary to list. Default \
                         'chat' (GGUF only). 'image' adds the stable-diffusion.cpp kinds.",
                        &["chat", "aux", "audio", "image"],
                    ),
                ),
            ],
            required: &["repo"],
        },
        Builtin {
            name: "lmgw__hf_downloads",
            writes: false,
            description:
                "Every tracked Hugging Face download with its status, destination path and \
                 live byte progress. Poll this after lmgw__hf_add until the status is 'done'.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__image_recipes",
            writes: false,
            description:
                "The image pipelines lmgw ships a recipe for — Z-Image-Turbo, FLUX.1 \
                 schnell/dev/Kontext, SDXL, Qwen-Image — and, for each, every file it needs: \
                 the Hugging Face repo, the exact filename, the size, whether the repo is \
                 licence-gated, the quantization alternatives, and whether that file is \
                 already on this box ('present') or downloading. START HERE for the image \
                 class. A stable-diffusion.cpp pipeline is NOT one weights file: it is a \
                 diffusion model (or an all-in-one checkpoint) plus a VAE plus one to three \
                 text encoders, and they live in DIFFERENT repos under different owners, so \
                 there is nothing lmgw__hf_repo can show you that describes a whole pipeline. \
                 Each recipe also carries the 'args' the family wants and a VRAM note — \
                 measured where a spike measured it, and the word 'unmeasured' where none \
                 did. Takes no arguments; the list is compiled in, so this makes no network \
                 call. Then: lmgw__image_recipe_add.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__usage",
            writes: false,
            description:
                "What was spent, on what: cost and token totals over a date range, with a \
                 ranked breakdown, from the same hourly rollups a usage dashboard reads — so \
                 this and any chart never disagree. Every total states its unpriced \
                 remainder beside it (design rule: a request with no price on file is never \
                 folded in as a silent zero, which would read as authoritative and be wrong \
                 downward) — see lmgw__prices for which aliases that currently affects and \
                 why. Dates are UTC; there is no browser here to read a timezone from.",
            props: vec![
                (
                    "from",
                    str_p(
                        "Start of the range: an ISO date (YYYY-MM-DD), or '<N>d' meaning N \
                         days before 'to' (e.g. '7d', '30d'). Default '30d'.",
                    ),
                ),
                (
                    "to",
                    str_p(
                        "End of the range (inclusive), an ISO date. Default today (UTC).",
                    ),
                ),
                (
                    "group_by",
                    enum_p(
                        "How to rank the breakdown; the tail beyond 'limit' folds into an \
                         'Other' row rather than being dropped. Default 'alias'.",
                        &["alias", "key", "class", "upstream", "none"],
                    ),
                ),
                (
                    "class",
                    enum_p(
                        "Restrict to one request class.",
                        &["chat", "aux", "audio", "image", "tool"],
                    ),
                ),
                ("alias", str_p("Restrict to one model alias.")),
                (
                    "limit",
                    int_p(
                        "Rows kept in the breakdown before the tail folds into 'Other'. \
                         Default 10.",
                    ),
                ),
            ],
            required: &[],
        },
        Builtin {
            name: "lmgw__prices",
            writes: false,
            description:
                "Every price sheet on file — catalog-synced and manual — per 1M tokens, plus \
                 `unpriced_models`: every model that currently resolves to no price at all, \
                 with the requests each has already spent unpriced. That list covers the \
                 configured aliases *and* the passthrough models an expose_all upstream \
                 serves, counted from the usage rollup — on a gateway with no configured \
                 aliases the passthrough models are the only ones there are. A manual row \
                 always wins over a catalog row for the same scope. An upstream that \
                 publishes no pricing at all (Gemini, for one) leaves its models unpriced \
                 unless priced by hand with lmgw__price_set; a local model is never in the \
                 list — it resolves free_local, not unknown.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__docs_corpora",
            writes: false,
            description:
                "The documentation corpora this gateway serves — one row per `library@version`, \
                 which is what the `docs__*` tools on /mcp answer agents from. Each carries the \
                 numeric id every other docs tool takes, its status, chunk and unembedded \
                 counts, the models it was ingested and embedded with, its eval score, the \
                 flags that say it is servable but degraded, its sources, and `job` — the \
                 ingest or re-embed run behind it, with stage and percent while it runs and the \
                 error if it failed. This is the poll after starting one. Per-document and \
                 per-chunk browsing is the dashboard's Docs tab, not this tool.",
            props: vec![(
                "corpus",
                str_p(
                    "Numeric corpus id or `library@version`. Omit for every corpus plus the \
                     pending doc-request count.",
                ),
            )],
            required: &[],
        },
        Builtin {
            name: "lmgw__docs_requests",
            writes: false,
            description:
                "The doc-request queue: what agents asked for through `docs__request` when the \
                 corpus they wanted was missing — library, version (empty when they did not pin \
                 one), the reason given, which client asked, how many times, and status. This \
                 is the worklist for a bulk import: read it, then create each corpus with \
                 lmgw__docs_corpus_set.",
            props: vec![(
                "status",
                enum_p(
                    "Filter by status. Omit for all of them.",
                    &["pending", "fulfilled", "dismissed"],
                ),
            )],
            required: &[],
        },
        Builtin {
            name: "lmgw__agents",
            writes: false,
            description:
                "The agent catalog: one row per installed agent — id, name, what it does, its \
                 run kind (`batch` is list → classify → review → apply; `chat` opens a \
                 tool-enabled thread), the model alias it resolves to, the MCP tool labels it \
                 attaches, whether it is enabled, where it came from, and `requires_ok` — false \
                 when a label it needs is not a registered MCP server on this gateway, in which \
                 case it cannot run until one is. An agent is a manifest, nothing is compiled \
                 in, so installing one is lmgw__agent_set with a JSON document.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__agent_get",
            writes: false,
            description:
                "One agent in full: its manifest as stored, its config values with every \
                 `secret` field replaced by `{has_value}`, the config form the manifest \
                 declares (each field's type, format, default, enum and bounds), which of its \
                 tool labels resolve right now, the tool-call and wall-clock budget a run is \
                 bounded by, and the run in flight if there is one. Read this before editing a \
                 manifest: lmgw__agent_set replaces the whole document, so start from this one.",
            props: vec![("id", str_p("Agent id, from lmgw__agents."))],
            required: &["id"],
        },
        Builtin {
            name: "lmgw__builds",
            writes: false,
            description:
                "Container builds: lmgw can build its own llama.cpp / ik_llama.cpp / audio.cpp / \
                 stable-diffusion.cpp server images from git — a repository and ref, plus PRs \
                 or other branches merged on top — and tags each result \
                 'localhost/lmgw-<engine-repo>:<slug>' (the MOVING tag, which follows the \
                 build's last verified run) and '…:<slug>-<base7>-<cfg6>' (immutable, one per \
                 run). Without id: every build with its newest run (last_run), the run its \
                 moving tag points at (current_run), the job id of a run in progress \
                 (live_job_id, null when idle) and who uses its moving tag (used_by: class \
                 defaults, model overrides, containers running or stopped) and its 'update' \
                 badge: null \
                 before the first update check (every build_update_check_hours; see \
                 lmgw__settings) or when it has no succeeded run to compare with, else \
                 {checked_at, reasons, ref_moved, extras, errors} — reasons like 'master +37 \
                 commits', 'master moved (abc1234 → def5678)', 'PR #1234 pushed', 'PR #1234 \
                 merged upstream and contained in your base — drop it' (only once the build's \
                 own base is proven to already hold the PR's merge or head commit; otherwise \
                 'PR #1234 merged upstream at <date>; your base doesn't contain it yet — it's \
                 still merged into your build'), 'PR #1234 closed unmerged', 'definition changed \
                 since last run (ref)'; empty reasons = up to date as of checked_at; what \
                 could not be checked (a forge rate limit with its reset time) is in errors. \
                 An update is acted on with lmgw__build_run. Also 'repo_presets' — the \
                 preset ids lmgw__build_set preset= accepts. With id: that build and its runs, \
                 newest first, each with its status (running; succeeded; unverified = built \
                 but not GPU-checked, e.g. under the GPU hold; broken = a verify check failed; \
                 failed; canceled; up_to_date = those exact inputs were already built and \
                 verified), resolved commits, image, tags, size, verify result and error. Poll \
                 this with id after lmgw__build_run until live_job_id is null.",
            props: vec![
                (
                    "id",
                    int_p("A build id: return that build and its run history instead of the list."),
                ),
                (
                    "limit",
                    int_p(
                        "With id: how many runs, newest first. Default 10; the answer's 'more' \
                         says whether older runs exist.",
                    ),
                ),
                (
                    "before",
                    int_p(
                        "With id: a run id — return the runs strictly older than it (the next \
                         page after the oldest one you have).",
                    ),
                ),
            ],
            required: &[],
        },
        Builtin {
            name: "lmgw__build_log",
            writes: false,
            description:
                "Read a build run's log — the full, never-truncated output of every phase \
                 (waiting, resolve, fetch, assemble, prepare, build, verify, promote, cleanup; \
                 each starts with a '==> ' header line). With neither 'offset' nor 'tail', it \
                 returns the log's LAST 200 LINES (a whole log is often megabytes) — where a \
                 failed run's reason is (also in the run's 'error' on lmgw__builds id=). 'tail' \
                 = N returns the last N lines instead. 'offset' = a byte position returns the \
                 text from there, at most 1 MiB per call (offset=0 reads from the beginning). \
                 Every answer has 'next_offset' — pass it as 'offset' on the next call to read \
                 on — and 'done', true once the run has ended and the answer reaches the end \
                 of the log. To follow a live run, call again with next_offset until done. \
                 Pass offset or tail, not both. Run ids come from lmgw__build_run or \
                 lmgw__builds id=.",
            props: vec![
                ("run_id", int_p("The run to read.")),
                (
                    "offset",
                    int_p(
                        "Byte offset to read on from (at most 1 MiB per call): 0 for the \
                         beginning, else the last next_offset. Omit (with tail) to get the \
                         last 200 lines.",
                    ),
                ),
                (
                    "tail",
                    int_p("Return the log's last N lines. Default when offset is omitted: 200."),
                ),
            ],
            required: &["run_id"],
        },
        Builtin {
            name: "lmgw__container_images",
            writes: false,
            description:
                "Every local podman image of the three engines (llama.cpp and ik_llama.cpp, \
                 audio.cpp, stable-diffusion.cpp) — images lmgw built, with their provenance \
                 (build, run, repository, ref, base commit, merged extras) and their run's \
                 status, and external ones (hand-built tags, registry pulls) — with tags, size, \
                 creation time, GPU backend label and who uses each: class defaults, model \
                 overrides, running and stopped containers, matched by image ID, so two \
                 names of one image are one image. A registry image in use (e.g. the ghcr.io \
                 audio and stable-diffusion defaults) carries 'registry_update': the digest \
                 the registry \
                 serves for its tag at the last update check against the digests podman holds, \
                 update_available, and an error when it could not be compared — pull an update \
                 with lmgw__container_image_pull. The 'disk' footer has podman's image totals \
                 and any buildah leftovers on disk, for information: nothing is pruned from \
                 here.",
            props: vec![(
                "engine",
                enum_p(
                    "Only this engine's images. Omit for all three.",
                    &["llama", "audio", "sdcpp"],
                ),
            )],
            required: &[],
        },
        Builtin {
            name: "lmgw__forge_prs",
            writes: false,
            description:
                "List a repository's open pull requests (GitHub) or merge requests (GitLab), \
                 most recently updated first, 100 per page — to find the number to put in a \
                 build's extras. Each has number, title, author, draft, state, head and base \
                 commit, merged_at and URL. A 'query' that is a number ('16391', '#16391') or \
                 a pasted PR URL returns that one PR whatever its state; other words filter \
                 the page (GitHub: every word must appear in the number, title or author; \
                 GitLab: server-side search), so a filtered page can be empty while next_page \
                 says more pages exist. Uses the forge token configured for the repository's \
                 host, if any; without one GitHub allows 60 requests an hour, and an exhausted \
                 quota is an error saying when it resets (rate_limit shows what is left).",
            props: vec![
                (
                    "id",
                    int_p("A build id: list the PRs of that build's repository, on its forge."),
                ),
                (
                    "repo_url",
                    str_p(
                        "The repository, when not naming a build: e.g. \
                         https://github.com/ggml-org/llama.cpp.",
                    ),
                ),
                (
                    "forge",
                    enum_p(
                        "Where the repository's PRs live. Default: the build's; for a repo_url, \
                         github for github.com (always), gitlab for a host with a configured \
                         forge token, otherwise plain — which has no PR list.",
                        &["github", "gitlab", "plain"],
                    ),
                ),
                (
                    "query",
                    str_p("A PR number or URL, or words to filter titles and authors by."),
                ),
                ("page", int_p("Page number, from 1 — the previous answer's next_page.")),
            ],
            required: &[],
        },
    ]
}
