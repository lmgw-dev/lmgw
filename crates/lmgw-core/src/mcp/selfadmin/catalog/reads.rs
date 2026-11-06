//! The read-only tools (`lmgw__status` … `lmgw__model_find`): every
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
                 reason ('prompt 41,210 + 8,192 > 30,000'). A runtime entry background traffic \
                 started (a background candidate alias, or a request sent with \
                 x-lmgw-background: on) carries owner: 'background' until an ordinary request \
                 claims it, absent otherwise. While any ordinary request waits for GPU room, \
                 every running entry carries draining_for_owner: true — one global mark, the \
                 owner is waiting for room and no model takes new background work; absent \
                 otherwise. \
                 When any candidate alias is configured, a candidate_aliases array lists each \
                 one's name, mode (background/owner), primary, alternates, enabled facets, \
                 problems and deferrals_24h — how many times it answered gpu_deferred (a \
                 background request declined rather than disturb ordinary traffic) in the last 24 \
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
                 also lists candidate aliases — 'kind': \
                 'candidate_alias' distinguishes them from a plain alias — each with its \
                 candidates list (primary first), background flag, enabled facets, which \
                 candidates are routable right now (routable_candidates), any problems (a candidate or the fallback \
                 that stopped supporting an enabled facet, say), and deferrals_24h — how many \
                 times a background alias answered gpu_deferred (declined rather than disturb \
                 ordinary traffic) in the last 24 hours. Local, aux, audio and image entries carry \
                 deferrals_24h too: requests sent for that name with x-lmgw-background: on that \
                 were deferred in the last 24 hours. Answering this makes the same cached catalog \
                 HTTP calls to expose-all upstreams that /v1/models makes (warm calls are free; \
                 a cold one fetches each reachable upstream's catalog once). Every model and \
                 alias entry carries fallback (the model or alias that answers when it cannot, \
                 as configured; absent for none) and fallback_usable (whether that name stands \
                 in right now).",
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
                 keys are never returned, only whether one is set. A llama_cpp row also \
                 carries llama_facts: what its server said in GET /props (build, slot \
                 context, modalities), when (read_at, unix seconds), or why it is unknown — \
                 one entry for the server (model empty) and, when it is a llama-server \
                 router (router: true), one per model. lmgw asks in the background on a \
                 row's first chat request or Test, and again after an edit, a transport \
                 failure, a media refusal or a Test.",
            props: vec![],
            required: &[],
        },
        Builtin {
            name: "lmgw__mcp_servers",
            writes: false,
            description:
                "List the MCP servers lmgw aggregates southbound, each with its transport, \
                 endpoint, live connection status, discovered tool count and open_tasks (the MCP \
                 tasks it runs that have not ended). Secrets in env \
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
                 why a fallback answered instead of the model or alias requested (hold, \
                 benchmark, external_vram, background, unavailable, quota, unreachable, \
                 disabled, gpu_setup, engine_download; along a chain, the last hop's); \
                 upstream_name is where the answer \
                 came from. An attempt handed to its fallback (an exhausted quota, an \
                 unreachable upstream) has a row of its own saying so. rung is the 1-based rung \
                 a ladder model's row was judged on (empty for a row without a ladder, and for \
                 one a fallback answered instead). service_tier is the tier an OpenAI \
                 platform answer reported (default, flex, priority, …), and price_note says \
                 why a row's cost is what it is when that is not plain (a tier with no price \
                 on file, an answer that ended before its usage was reported). Use it to \
                 diagnose a failing model or upstream.",
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
                 mode, the four container configurations (chat, aux, audio, image — audio \
                 also carries catalog_revision, which revision an audio catalog download \
                 takes), the GPU hold (active), the external-VRAM fallback switch \
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
                 (embedding / rerank / decision), image (stable-diffusion.cpp) or audio \
                 (audio.cpp) — \
                 every parameter, whether each file it references actually exists on disk, \
                 static problems, and the exact command line its container is started with. \
                 Use this to read back what you just wrote; lmgw__models only lists names. A \
                 model id is looked up in the chat table first, then aux, then image, then \
                 audio; the result says which ('class'). An aux row also carries the thread \
                 count its container starts with and where it comes from (threads_in_effect, \
                 threads_source: row, args, cores, logical_cpus or llama_server). An image row \
                 comes back with its \
                 files/args maps, its modes and edit flag, and a files_present map saying \
                 which paths resolve. An audio row comes back with every field, whether its \
                 model directory is present, the server.json its container is started with \
                 (audio.cpp takes no command-line flags), the image and run args in effect, \
                 whether it runs, and its learned GPU residency with the sentence saying what \
                 admission charges it. A \
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
                        "Which class to look in. Omit to search chat, then aux, then image, \
                         then audio.",
                        &["chat", "aux", "image", "audio"],
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
                 default; target=aux for the embedding / rerank / decision dir), with sizes \
                 and whether \
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
                         'aux' is the embedding / rerank / decision models dir, 'image' the \
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
                 embedding, rerank or decision model is an aux model, recognised by the \
                 pooling_type, classifier head or decision.type in its header, whatever \
                 directory it sits in), \
                 and estimates KV cache cost. Reads only the file header and loads \
                 nothing: whether the image can load the model shows on \
                 lmgw__local_model_test.",
            props: vec![
                (
                    "gguf_path",
                    str_p("GGUF path relative to the target's models directory."),
                ),
                (
                    "target",
                    enum_p(
                        "Which class's models directory the path is under. Default 'chat'. \
                         'aux' is the embedding / rerank / decision models dir — the four \
                         classes keep \
                         separate trees, so a file downloaded with lmgw__hf_add target=aux is \
                         only visible here with target=aux. 'image' is the \
                         stable-diffusion.cpp tree, whose files are mostly not GGUF: only a \
                         .gguf there has a header to read.",
                        &["chat", "aux", "audio", "image"],
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
                 embedding, rerank or decision model (recognised from the header, not the \
                 filename) gets kind, pooling, ctx_size and, where the header calls for \
                 them, ubatch_size, batch_size, mmproj_path and backend for \
                 lmgw__aux_model_set instead — pass \
                 target=aux for a file in the aux models dir. Every value carries a \
                 rationale, and warnings name combinations that would fail to load, \
                 including a GPU plan whose estimated footprint does not fit the card \
                 (with the largest ctx_size that does; the plan keeps the trained \
                 context). It reads the header only and loads nothing; \
                 lmgw__local_model_test is what loads a model. Start \
                 here instead of guessing llama.cpp parameters. If a configured model \
                 already uses this GGUF, 'configured_as' additionally reports how each one \
                 differs from the plan and carries a ready-made update patch — so this is \
                 also the repair tool, not just the add-a-model tool. For target=image the \
                 result's params are lmgw__image_model_set's, matched from the shipped \
                 recipe list rather than read out of the file. A file not downloaded yet is \
                 planned from Hugging Face instead: pass repo and file in place of gguf_path, \
                 and lmgw reads the file's header (and its projector's and drafter's) from \
                 the hub, which costs 1 to 16 MiB a file and downloads nothing; the plan is \
                 the one the file gets once lmgw__hf_add has put it on disk. A chat or aux \
                 plan also answers 'source' and 'fit': whether the file fits the graphics \
                 card, measured against the whole card and beside what runs on it now, with \
                 the largest context that fits. An image plan answers neither yet.",
            props: vec![
                (
                    "gguf_path",
                    str_p(
                        "Weights GGUF path relative to the target's models directory. Or name \
                         a file on Hugging Face with repo and file instead.",
                    ),
                ),
                (
                    "repo",
                    str_p(
                        "A Hugging Face repo (owner/name) to plan a file of before downloading \
                         it, with file; in place of gguf_path.",
                    ),
                ),
                (
                    "file",
                    str_p(
                        "The weights GGUF's path in repo, as lmgw__hf_repo lists it (the \
                         first part of a split file).",
                    ),
                ),
                (
                    "target",
                    enum_p(
                        "Which class's models directory the path is under. Default 'chat'; with repo \
                         and file, the class to plan for, else the header's. \
                         'aux' is the embedding / rerank / decision models dir — the classes \
                         keep \
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
            ],
            required: &[],
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
                 into one entry. A repo with nothing to list answers an empty `files` and a `note` saying so. Call this before lmgw__hf_add to choose a quantization. \
                 With target=image it lists .safetensors / .ckpt / .pt / .pth as well as \
                 .gguf, and names the roles of an image pipeline instead (diffusion, \
                 checkpoint, vae, text_encoder, lora, upscaler) — but one image pipeline \
                 spans several repos, so prefer lmgw__image_recipes, which names every \
                 component of a known family with its repo, file and size. \
                 With fit=true each 'weights' file also gets its fit on this computer's \
                 graphics card (`fit`: by the whole card and beside what runs on it now, with \
                 the longest conversation that fits) and the answer names the `recommended` \
                 file with its reason (`picks`). That reads GGUF headers from Hugging Face, \
                 1 to 16 MiB each: one file's, then only the files near the card's edge and \
                 those that could be the recommended one; the others are estimated from it \
                 (`estimated_from`), never smaller than the real use. `header_bytes_read` \
                 says what it cost. The card measured (`card`) and what holds for every file \
                 (`fit_notes`: the hold, admission, where the estimates came from, why the reads \
                 stopped) are said once for the list, not in each file's fit. A gated repo, the \
                 hub's rate limit or no connection stops the reads at the first refusal; nothing \
                 is recommended then. lmgw__local_model_plan with repo + file reads one file \
                 exactly.",
            props: vec![
                (
                    "repo",
                    str_p(
                        "Repo id like 'unsloth/Qwen3.5-9B-GGUF' — owner/name only, not a \
                         full huggingface.co URL.",
                    ),
                ),
                (
                    "fit",
                    bool_p(
                        "Mark the weights files with their fit on the graphics card and name \
                         the recommended one; reads file headers (1 to 16 MiB each). Chat and \
                         aux only. Default false: nothing is read.",
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
                "Tracked Hugging Face downloads with their status, destination path and \
                 live byte progress. Poll this after lmgw__hf_add, lmgw__image_recipe_add, \
                 lmgw__image_upscaler_add or an audio catalog download with ids = the \
                 download ids that call returned, until each status is 'done'. Without ids \
                 it returns every row ever recorded, which grows without bound; `active` \
                 true keeps only the rows that still need attention (status queued, \
                 downloading or failed; done and update_available are left out). With both, \
                 a row must match both. With neither, every row. \
                 requested_revision is what the download asks for ('main', or the commit an \
                 audio.cpp spec pins, which an audio catalog download takes under \
                 audio.catalog_revision 'pinned'); resolved_commit is the commit the file on \
                 disk came from, as the hub named it. On a row from before lmgw recorded \
                 them, requested_revision is null because every download then took main, \
                 and resolved_commit is null because the commit is unknown.",
            props: vec![
                (
                    "ids",
                    serde_json::json!({
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Only these download row ids: the ids hf_add, \
                            image_recipe_add, image_upscaler_add or an audio catalog \
                            download returned.",
                    }),
                ),
                (
                    "active",
                    bool_p(
                        "true: only rows that are not finished (status queued, downloading \
                         or failed).",
                    ),
                ),
            ],
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
                "What was spent, on what: cost, token and measured-quantity totals (input \
                 audio, input characters, generated images) over a date range, with a \
                 ranked breakdown, from the same hourly rollups a usage dashboard reads — so \
                 this and any chart never disagree. Every total states its unpriced \
                 remainder beside it (a request with no price on file is never \
                 folded in as a silent zero, which would read as authoritative and be wrong \
                 downward) — see lmgw__prices for which aliases that currently affects and \
                 why. Dates are UTC; there is no browser here to read a timezone from. The answer \
                 echoes the window as 'from' (inclusive) and 'to' (exclusive) ISO 8601 UTC \
                 instants, and the class, alias and group_by that were applied.",
            props: vec![
                (
                    "from",
                    str_p(
                        "Start of the range: an ISO date (YYYY-MM-DD), or '<N>d' meaning N \
                         days ending with 'to' (e.g. '7d' is 'to' and the 6 days before it). \
                         Default '30d'.",
                    ),
                ),
                (
                    "to",
                    str_p(
                        "Last day of the range, inclusive, an ISO date (YYYY-MM-DD). Default \
                         today (UTC). The answer's 'to' is the exclusive end instant: the \
                         start of the day after this one.",
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
                "Every price sheet on file — catalog-synced and manual — each in one billable \
                 unit (`unit`): per_mtok rows carry four token rates per 1M tokens, every \
                 other unit one `price` per its scale (per minute of input audio, per 1M \
                 input characters, per generated image, per answered request). Plus \
                 `unpriced_models`: every model for which no unit resolves a price at all, \
                 each as {name, requests} — the requests it has already spent unpriced. \
                 That list covers the \
                 configured aliases *and* the passthrough models an expose_all upstream \
                 serves, counted from the usage rollup — on a gateway with no configured \
                 aliases the passthrough models are the only ones there are. A manual row \
                 always wins over a catalog row for the same scope. An upstream that \
                 publishes no pricing at all (Gemini, for one) leaves its models unpriced \
                 unless priced by hand with lmgw__price_set; a local model is never in the \
                 list — it resolves free_local, not unknown. Every sheet names its \
                 service_tier (OpenAI's pricing tiers; standard for every other provider), and \
                 `unpriced_tiers` lists each model that answered at a tier it has no row for, \
                 as {name, scope_kind, scope_key, service_tier, requests, since}: that traffic \
                 is unpriced, and counts toward no budget, until a row of that tier exists. \
                 `unpriced_hosted_tools` lists each model whose native /v1/responses answers \
                 ran a hosted tool that bills beyond its tokens (web search, file search, a \
                 container, image generation), as {name, scope_key, calls, requests, since}: \
                 those rows are unpriced, since no price unit covers the fees yet.",
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
                 bounded by, the run in flight if there is one (live_job) and the latest run's outcome \
                 (last_run). Read this before editing a \
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
                 repository presets (the id is what lmgw__build_set preset= takes). With id: that build and its runs, \
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
        Builtin {
            name: "lmgw__machine_check",
            writes: false,
            description:
                "Whether this computer can run local models, item by item: the graphics card and \
                 its driver (the running kernel module against the driver libraries on disk, NVML \
                 read in a separate process), what lets models use the card (who owns \
                 /usr/bin/nvidia-ctk and whether lmgw could write its own CDI spec), the program \
                 that runs models (Podman), whether the model settings fit the card, free disk \
                 space and whether the engine images are downloaded. Each item has a plain \
                 sentence for the person, technical detail lines and a typed cause; the verdict \
                 is ready, needs_fix, needs_steps, needs_restart, needs_driver or cloud_only. \
                 When something can be done, the answer carries the plan: every step with the \
                 exact commands it runs as root, the full text of the files it writes, the same \
                 lines to type in a terminal, and how it is carried out (the app's button or \
                 instructions, and why). The fix itself runs only from lmgw's own app window on \
                 this computer, after the person confirms and types their password: explain the \
                 plan to them, but it cannot be run from here. When the model settings are for \
                 the other maker's card, the settings item carries an action instead: every \
                 field the switch to the settings for this card changes, as it is and as it \
                 would become, what it leaves, and the running models it stops; that switch \
                 needs no password and is the machine_settings_fit op (POST \
                 /api/op/machine_settings_fit, with confirm set to the listed changes), which \
                 is not a tool here. Without fresh this is the last \
                 result (the check runs at boot); fresh: true runs it now, joining a run already \
                 in progress that began after the call. It only reads, with the user's rights, \
                 and runs these programs: lmgw's own NVML probe (lmgw --machine-probe nvml, in \
                 a process of its own), rpm and rpmkeys queries, nvidia-ctk --version, one \
                 cache-only dnf repoquery for the toolkit's download sizes, and Podman's \
                 --version, --help, info and image exists for each engine image, and podman \
                 images; for an engine image that is not one of the engines' published images \
                 it reads its build label, or lists its ggml libraries in a throwaway \
                 container with no network and no device, to tell which card it is built for; \
                 for an engine image that is not downloaded yet it reads its registry manifest \
                 anonymously, for the download's size.",
            props: vec![(
                "fresh",
                bool_p("Run the check now instead of answering the last result."),
            )],
            required: &[],
        },
        Builtin {
            name: "lmgw__model_find",
            writes: false,
            description:
                "Finds a model by the name someone said ('qwen 3.6'), a Hugging Face address (a repo, a \
                 file, a download link, also inside pasted text) or owner/repo, and picks one model and one \
                 file of it that fits this computer's graphics card. Names are searched as typed, with no \
                 aliases: numbers are written as digits ('gemma 4', not 'gemma four'). It asks Hugging \
                 Face's model search twice (every repo, and GGUF repos, each the hub's first page by \
                 downloads); keeps the repos whose name has every query word; takes the originals, not \
                 uploads of them; ranks them by matched words, weights of their own, likes and downloads; \
                 and walks the top author's originals of the top one's kind to the first whose upload has a \
                 file that fits by its size (an upload by the maker first, gated ones last); when none \
                 fits, the card is the one closest to fitting (nothing_fits). It reads no file header, so \
                 the card's marks are by size ('fits_by_size'): lmgw__hf_repo with fit=true on the card's \
                 upload reads the header (1 to 16 MiB) and checks exactly. A search usually costs 3 to 8 \
                 Hugging Face requests: the two searches, two per model the walk reads (its uploads, a \
                 tree) plus one per gated or unanswered upload or further tree page, and the display \
                 name; lmgw sets no bound of its own, the hub's limit is the bound. hub_budget carries \
                 the hub's own RateLimit and RateLimit-Policy figures, near when fewer requests are left \
                 than this answer took; a limit used up before anything was found answers rate_limited \
                 with the seconds to the reset, and one used up partway keeps what was found, with a \
                 note. An address on another host, or a Hugging Face page that isn't a model's \
                 (not_a_model), is answered without asking anyone. A private repo's card has no share \
                 link. others, did_you_mean and the card's other_uploads are windows of 10 by default \
                 (each one's *_total and *_more say how many there are); the *_offset and *_count \
                 arguments ask for another window, which runs the same search again. \
                 next_cursor continues the search with the hub's next pages (answer 'more': the lists \
                 only).",
            props: vec![
                (
                    "query",
                    str_p(
                        "A model's name as people say it ('qwen 3.6'), a Hugging Face address, \
                         or owner/repo.",
                    ),
                ),
                (
                    "cursor",
                    str_p("A previous answer's next_cursor: the search's next pages."),
                ),
                (
                    "others_offset",
                    int_p("The first entry of others to answer with (default 0)."),
                ),
                (
                    "others_count",
                    int_p("How many entries of others to answer with (default 10)."),
                ),
                (
                    "did_you_mean_offset",
                    int_p("The first entry of did_you_mean to answer with (default 0)."),
                ),
                (
                    "did_you_mean_count",
                    int_p("How many entries of did_you_mean to answer with (default 10)."),
                ),
                (
                    "other_uploads_offset",
                    int_p("The first entry of the card's other_uploads to answer with (default 0)."),
                ),
                (
                    "other_uploads_count",
                    int_p("How many entries of the card's other_uploads to answer with (default 10)."),
                ),
            ],
            required: &["query"],
        },
    ]
}
