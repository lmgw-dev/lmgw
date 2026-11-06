//! Where models and MCP servers come from: `lmgw__mcp_server_set`,
//! `lmgw__model_get`, `lmgw__model_get_status`,
//! `lmgw__hf_add`, `lmgw__image_recipe_add`, `lmgw__hf_set`.

use crate::mcp::selfadmin::{bool_p, enum_p, int_p, str_p, strs_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__mcp_server_set",
            writes: true,
            description:
                "Create, update, delete, enable, disable or test a southbound MCP server that \
                 lmgw aggregates. extra_run_args is a list of strings, one per \
                 argument; the other list-valued fields are newline-delimited text: args one \
                 per line, env as KEY=VALUE lines, headers as 'Name: Value' lines. Omit env/headers on update to keep the stored values. \
                 A call that moves url to another address (update, enable or disable) must \
                 pass headers as text in the same call when the server holds any (\"\" sends \
                 none; null keeps the stored ones, so it does not count), or it is refused \
                 and nothing changes: stored headers, which may hold its credential, are \
                 never sent to a host they were not given for. A successful change \
                 reconnects the server and pushes tools/list_changed.",
            props: vec![
                (
                    "action",
                    enum_p(
                        "What to do. 'test' connects once and reports the tool count.",
                        &["create", "update", "delete", "enable", "disable", "test"],
                    ),
                ),
                ("id", int_p("MCP server id. Required except on create.")),
                ("name", str_p("Display name.")),
                (
                    "transport",
                    enum_p(
                        "How to reach the server. Required on create.",
                        &["stdio", "http", "sse"],
                    ),
                ),
                (
                    "url",
                    str_p(
                        "Endpoint URL for http/sse transports. Moving it needs headers, as \
                         text, in the same call when the server holds any.",
                    ),
                ),
                (
                    "command",
                    str_p("Executable for stdio transport (bare, or inside the container)."),
                ),
                ("args", str_p("Command arguments, one per line.")),
                (
                    "env",
                    str_p("Environment for the server process, KEY=VALUE one per line."),
                ),
                ("cwd", str_p("Working directory for a bare stdio subprocess.")),
                (
                    "container_image",
                    str_p("Podman image to run the stdio server isolated in. Recommended."),
                ),
                (
                    "extra_run_args",
                    strs_p("Extra 'podman run' arguments, one per entry."),
                ),
                (
                    "headers",
                    str_p("HTTP headers for http/sse, 'Name: Value' one per line."),
                ),
                (
                    "tool_prefix",
                    str_p(
                        "Namespace for this server's tools: prefix 'gh' exposes 'gh__search'. \
                         'lmgw' is reserved.",
                    ),
                ),
                (
                    "timeout_ms",
                    int_p(
                        "Timeout in ms for each tool call and for the connect handshake. Not \
                         bounded by it: the pull of a container image before the handshake, and \
                         the whole start of a stdio server without a container image (an npx \
                         or uvx command may install its package first). Default 60000.",
                    ),
                ),
                ("autostart", bool_p("Connect at startup instead of on first use.")),
                (
                    "idle_seconds",
                    int_p("Disconnect after this many idle seconds. 0 = stay connected."),
                ),
                (
                    "allow_sampling",
                    bool_p("Let this server ask lmgw for LLM completions (MCP sampling)."),
                ),
                (
                    "sampling_alias",
                    str_p("Model alias to answer this server's sampling requests with."),
                ),
                ("enabled", bool_p("Whether the server is aggregated at all.")),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__hf_add",
            writes: true,
            description:
                "Download a model from Hugging Face into the models directory — the normal \
                 way to get a new model onto this gateway. Fetches the weights file you \
                 name (or pick by quantization) together with its multimodal projector and, \
                 for target=chat, its speculative drafter from the same directory, because a \
                 model usually needs all of them. Split GGUFs expand to every part \
                 automatically. Downloads run \
                 in the background: poll lmgw__hf_downloads with ids=<the queued ids>, then call \
                 lmgw__local_model_plan on the result. target=image downloads the \
                 stable-diffusion.cpp kinds (.safetensors, .ckpt, .pt, .pth as well as \
                 .gguf) into the image models dir and fetches no companions — an image \
                 pipeline's VAE and text encoders live in OTHER repos, so use \
                 lmgw__image_recipe_add to bring a whole family down in one call and this \
                 tool only for a single file a recipe does not cover. When the image the \
                 target class's models run in (its Settings → Runtimes Image) is not on this \
                 machine, this also starts its download, a multi-gigabyte image_pull job of \
                 its own that starts no model: 'engine' in the answer names the image and \
                 the job (the model's first start joins it rather than downloading again), \
                 or says why it could not begin or why podman could not say whether it is \
                 needed; 'engine' is null when the image is here.",
            props: vec![
                (
                    "repo",
                    str_p(
                        "Repo id like 'unsloth/Qwen3.5-9B-GGUF' — owner/name only, not a \
                         full huggingface.co URL.",
                    ),
                ),
                (
                    "file",
                    str_p(
                        "Exact path of the weights file within the repo. Omit to select by \
                         quant instead.",
                    ),
                ),
                (
                    "quant",
                    str_p(
                        "Pick the weights file by quantization label instead of exact name, \
                         e.g. 'Q4_K_XL' or 'IQ4_XS'. See lmgw__hf_repo for what is offered.",
                    ),
                ),
                (
                    "target",
                    enum_p(
                        "Which models directory to download into. Default 'chat'. Use 'aux' \
                         for embedding, rerank and decision models and 'image' for \
                         stable-diffusion.cpp \
                         pipeline files — a row can only address files in its own class's \
                         directory, and this is what keeps the file in the updater's view \
                         (lmgw__hf_set check_updates).",
                        &["chat", "aux", "audio", "image"],
                    ),
                ),
                (
                    "companions",
                    bool_p(
                        "Also fetch the projector beside the weights, and for target=chat \
                         the drafter (an aux row has none). Default true; set false to \
                         download only the file you named.",
                    ),
                ),
            ],
            required: &["repo"],
        },
        Builtin {
            name: "lmgw__model_get",
            writes: true,
            description:
                "Get a llama.cpp model from Hugging Face and set it up, sized to this \
                 machine's graphics card: the one call after choosing a file (lmgw__model_find, \
                 or lmgw__hf_repo with fit=true for the exact pick). It downloads the weights \
                 with the companions lmgw__hf_add would fetch (the one projector, and for chat \
                 the drafter; a file already downloaded isn't fetched again), then, in the \
                 background, plans the file on disk, fits it to the graphics card and creates \
                 the row: when the card fits a shorter conversation than the model's trained \
                 context, the row takes the largest that fits and keeps a note of the fit (an \
                 aux row is planned again at that context, its ubatch and batch with it). A row \
                 that already serves the file is the answer instead of a second one: then \
                 row_id is set, nothing is queued and there is no get. It never starts the \
                 model. The answer has the get_id (lmgw__model_get_status reads how it stands \
                 and, at the end, the row it made or why it stopped), the download ids for \
                 lmgw__hf_downloads, the model id and the name it will answer to, and the \
                 free-disk check (a warning, never a block). A second call for the same file \
                 joins the first. A failed get is started again by calling this again: it \
                 fetches only what isn't there yet. It survives a restart: the downloads go on \
                 from where they stopped and the setup follows.",
            props: vec![
                (
                    "repo",
                    str_p("Repo id like 'unsloth/Qwen3.5-9B-GGUF' — owner/name only."),
                ),
                (
                    "file",
                    str_p(
                        "Exact path of the weights file in the repo; for a split GGUF its \
                         first part.",
                    ),
                ),
                (
                    "target",
                    enum_p(
                        "The class to set it up in. Absent: the file's header decides (an \
                         embedder, a reranker or a decision model is aux, read from the \
                         header before the download, 1 to 16 MiB).",
                        &["chat", "aux"],
                    ),
                ),
                (
                    "measure",
                    enum_p(
                        "What the context is fitted to: 'beside' what runs on the card now \
                         (the default), or the 'whole_card' with nothing else on it.",
                        &["beside", "whole_card"],
                    ),
                ),
                (
                    "model_id",
                    str_p(
                        "The model's id. Absent: the repo's name in lower case without a \
                         trailing -GGUF. A name a row or an alias already holds on /v1 gets \
                         -2, -3 and so on; one an upstream passes through to its own models \
                         is refused, naming that upstream.",
                    ),
                ),
            ],
            required: &["repo", "file"],
        },
        Builtin {
            name: "lmgw__model_get_status",
            writes: false,
            description:
                "How one Get it stands (the get_id lmgw__model_get answered): running, done, \
                 failed or canceled; a running get's stage (downloading, paused, setting up, \
                 ready) with each file that waits and why (waiting: metered, not_started, or \
                 other with the download's note) and its bytes so far; every file's download row; a \
                 failed get's sentence, which says what to do; and a finished get's result: \
                 the row it created or found (row_id, model_id, the name it answers to on /v1), \
                 its context, the fit and the note, and the next step. Poll it after \
                 lmgw__model_get until the state isn't running.",
            props: vec![("get_id", int_p("The get, as lmgw__model_get answered it."))],
            required: &["get_id"],
        },
        Builtin {
            name: "lmgw__image_recipe_add",
            writes: true,
            description:
                "Download every component of one shipped image pipeline and get back a \
                 PREFILLED ROW for it. The whole chain: lmgw__image_recipes to choose a key \
                 -> THIS TOOL -> lmgw__hf_downloads ids=<the queued ids> until every one says 'done' -> \
                 lmgw__image_model_set action=create with the returned 'row' (it is already \
                 in that tool's field names: model_id, files, args, modes, edit) -> \
                 lmgw__local_model_test model_id=<id> target=image. Components already on \
                 disk are skipped, not re-fetched — a VAE or text encoder shared with a \
                 pipeline you already have costs nothing. It does NOT create the row itself: \
                 the files are still transferring, and lmgw__image_model_set refuses a row \
                 whose files are not on disk, which is the check that keeps a broken row out \
                 of the table. Refuses cleanly when the image models directory is not \
                 configured, or when a component's repo is licence-gated and no Hugging Face \
                 token is set (naming the component). When the image class's image (Settings \
                 → Runtimes → Image → Image) is not on this machine, this also starts its \
                 download, a multi-gigabyte image_pull job of its own that starts no model; \
                 'engine' in the answer names the image and the job, or says why it could \
                 not begin or why podman could not say whether it is needed, and is null \
                 when the image is here.",
            props: vec![
                (
                    "key",
                    str_p(
                        "Recipe key from lmgw__image_recipes, e.g. 'z-image-turbo' or \
                         'flux1-schnell'.",
                    ),
                ),
                (
                    "diffusion_file",
                    str_p(
                        "Pick a different quantization of the pipeline's diffusion model (or \
                         checkpoint) by exact filename — one of the 'alternatives' that \
                         recipe's primary component lists. Omit for the recipe's default. \
                         A name the recipe does not know is refused rather than ignored.",
                    ),
                ),
            ],
            required: &["key"],
        },
        Builtin {
            name: "lmgw__image_upscaler_add",
            writes: true,
            description:
                "Download a shipped ESRGAN upscaler (upscalers.shipped of \
                 lmgw__image_recipes) into upscalers/ of the image models dir, where every \
                 image model's server finds it. Then: lmgw__hf_downloads ids=[<its download id>] until it says \
                 'done'; an image model sees it from its next start, and image__upscale and \
                 the Chat card's Upscale offer it by its file stem. A file already there or \
                 already downloading is not queued again.",
            props: vec![(
                "key",
                str_p("Upscaler key from upscalers.shipped, e.g. 'realesrgan-x4plus'."),
            )],
            required: &["key"],
        },
        Builtin {
            name: "lmgw__hf_set",
            writes: true,
            description:
                "Manage tracked Hugging Face downloads: re-fetch one, cancel a running \
                 transfer, untrack and delete its file, or ETag-check every completed \
                 download against the hub for updates. An audio catalog file is checked and \
                 re-fetched at the revision audio.catalog_revision takes it at now: under \
                 'pinned' the commit its audio.cpp spec pins today, else main.",
            props: vec![
                (
                    "action",
                    enum_p(
                        "What to do. 'cancel' stops a transfer that is still running; the \
                         entry stays tracked and can be re-fetched with 'redownload'.",
                        &["redownload", "cancel", "delete", "check_updates"],
                    ),
                ),
                (
                    "id",
                    int_p("Download row id from lmgw__hf_downloads. Required except for check_updates."),
                ),
                (
                    "target",
                    enum_p(
                        "Which models directory check_updates scans. Default 'chat'.",
                        &["chat", "aux", "audio", "image"],
                    ),
                ),
            ],
            required: &["action"],
        },
    ]
}
