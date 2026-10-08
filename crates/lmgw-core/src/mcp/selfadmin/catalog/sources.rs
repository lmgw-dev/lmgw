//! Where models and MCP servers come from: `lmgw__mcp_server_set`,
//! `lmgw__hf_add`, `lmgw__image_recipe_add`, `lmgw__hf_set`.

use crate::mcp::selfadmin::{bool_p, enum_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__mcp_server_set",
            writes: true,
            description:
                "Create, update, delete, enable, disable or test a southbound MCP server that \
                 lmgw aggregates. List-valued fields are newline-delimited text: args and \
                 extra_run_args one per line, env as KEY=VALUE lines, headers as \
                 'Name: Value' lines. Omit env/headers on update to keep the stored values. \
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
                    str_p("Extra 'podman run' arguments, one per line."),
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
                 name (or pick by quantization) together with its multimodal projector and \
                 speculative drafter from the same directory, because a model usually needs \
                 all of them. Split GGUFs expand to every part automatically. Downloads run \
                 in the background: poll lmgw__hf_downloads, then call \
                 lmgw__local_model_plan on the result. target=image downloads the \
                 stable-diffusion.cpp kinds (.safetensors, .ckpt, .pt, .pth as well as \
                 .gguf) into the image models dir and fetches no companions — an image \
                 pipeline's VAE and text encoders live in OTHER repos, so use \
                 lmgw__image_recipe_add to bring a whole family down in one call and this \
                 tool only for a single file a recipe does not cover.",
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
                         for embedding and rerank models and 'image' for stable-diffusion.cpp \
                         pipeline files — a row can only address files in its own class's \
                         directory, and this is what keeps the file in the updater's view \
                         (lmgw__hf_set check_updates).",
                        &["chat", "aux", "audio", "image"],
                    ),
                ),
                (
                    "companions",
                    bool_p(
                        "Also fetch the projector and drafter beside the weights. Default \
                         true; set false to download only the file you named.",
                    ),
                ),
            ],
            required: &["repo"],
        },
        Builtin {
            name: "lmgw__image_recipe_add",
            writes: true,
            description:
                "Download every component of one shipped image pipeline and get back a \
                 PREFILLED ROW for it. The whole chain: lmgw__image_recipes to choose a key \
                 -> THIS TOOL -> lmgw__hf_downloads until every queued id says 'done' -> \
                 lmgw__image_model_set action=create with the returned 'row' (it is already \
                 in that tool's field names: model_id, files, args, modes, edit) -> \
                 lmgw__local_model_test model_id=<id> target=image. Components already on \
                 disk are skipped, not re-fetched — a VAE or text encoder shared with a \
                 pipeline you already have costs nothing. It does NOT create the row itself: \
                 the files are still transferring, and lmgw__image_model_set refuses a row \
                 whose files are not on disk, which is the check that keeps a broken row out \
                 of the table. Refuses cleanly when the image models directory is not \
                 configured, or when a component's repo is licence-gated and no Hugging Face \
                 token is set (naming the component).",
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
