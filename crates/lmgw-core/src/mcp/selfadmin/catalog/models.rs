//! Local llama.cpp model tools: `lmgw__local_model_set` through
//! `lmgw__local_model_test`, covering chat, aux (embed/rerank) and
//! image-model rows plus the load smoke test.

use crate::mcp::selfadmin::{bool_p, enum_p, int_p, ladder_p, num_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__local_model_set",
            writes: true,
            description:
                "Create, update, delete, enable, disable or duplicate a local llama.cpp \
                 CHAT model entry. Every llama-server parameter lmgw manages is settable \
                 here. Embedding and rerank models are a different class with their own \
                 directory and tool — lmgw__aux_model_set — and must not be created here \
                 (a chat row cannot serve /v1/embeddings). \
                 Prefer lmgw__local_model_plan to derive these values from the GGUF itself \
                 rather than guessing them. Update is partial: pass only what changes, and \
                 use 'clear' to unset a field. Duplicate clones the model named by id/ \
                 model_id under a fresh '<id>-copy' id (GGUF, params, args and flags carry \
                 over verbatim) — a quick way to try a different configuration (reasoning \
                 vs not, \
                 a different slot/context split) against the same weights. A model's own \
                 container starts automatically on its first request; if it might already be \
                 running under stale config, run lmgw__container model=<id> action=apply to \
                 recreate it explicitly. lmgw__local_model_test then confirms the model really \
                 loads (it starts the container itself, so it works either way).",
            props: vec![
                (
                    "action",
                    enum_p(
                        "What to do.",
                        &["create", "update", "delete", "enable", "disable", "duplicate"],
                    ),
                ),
                ("id", int_p("Local model row id.")),
                (
                    "model_id",
                    str_p("Client-facing model id; also selects the row when id is omitted."),
                ),
                (
                    "gguf_path",
                    str_p(
                        "Weights GGUF, relative to the models directory (e.g. \
                         'unsloth/Qwen3.5-9B-GGUF/Qwen3.5-9B-UD-Q4_K_XL.gguf'). Required on \
                         create. Prefer lmgw__hf_add to obtain one: it fetches the projector \
                         and drafter along with the weights, which this route cannot \
                         discover. lmgw__gguf_files lists what is already on disk.",
                    ),
                ),
                // -- multimodal --
                (
                    "mmproj_path",
                    str_p(
                        "Multimodal vision/audio projector GGUF (--mmproj), relative to the \
                         models directory exactly like gguf_path. Required for a multimodal \
                         model to see images; without it the model loads text-only.",
                    ),
                ),
                (
                    "no_mmproj",
                    bool_p(
                        "Refuse to auto-load a projector sitting next to the weights \
                         (--no-mmproj). For the text-only twin of a multimodal repo.",
                    ),
                ),
                // -- memory & placement --
                (
                    "ctx_size",
                    int_p(
                        "Context length (--ctx-size). Use the model's real trained context \
                         from lmgw__model_inspect; do not pick a smaller round number \
                         defensively.",
                    ),
                ),
                (
                    "n_predict",
                    int_p(
                        "Maximum tokens one response may generate (--n-predict). This is \
                         what /v1/models reports as max_output_tokens; omit for \
                         llama-server's unbounded default rather than guessing a cap. -1 = \
                         unbounded.",
                    ),
                ),
                (
                    "n_gpu_layers",
                    int_p("Layers to offload to the GPU (--n-gpu-layers). 999 = all."),
                ),
                ("threads", int_p("CPU threads (--threads).")),
                ("batch_size", int_p("Logical batch size (--batch-size).")),
                ("ubatch_size", int_p("Physical batch size (--ubatch-size).")),
                (
                    "parallel",
                    int_p("Concurrent request slots (--parallel). Splits the context."),
                ),
                (
                    "kv_unified",
                    bool_p(
                        "One shared KV pool across every slot instead of one pool per slot \
                         (--kv-unified true / --no-kv-unified false), so a single conversation \
                         can use the whole context instead of being capped to ctx_size / \
                         parallel. llama-server aborts every running request when a shared \
                         pool overflows, so lmgw refuses to save this as true with more than \
                         one effective slot unless n_predict is also set (it bounds the \
                         reservation the pool ledger guards the row with). Omit to leave \
                         llama-server's own default, which is unified exactly when parallel is \
                         also omitted.",
                    ),
                ),
                (
                    "kv_unified_per_slot",
                    int_p(
                        "Per-request context cap on a unified row, and — only when ctx_size is \
                         unset — how large the shared pool is sized (--kv-unified-per-slot, \
                         parallel x this). Meaningless on a split row (kv_unified effectively \
                         off), so it is refused there; must be positive.",
                    ),
                ),
                (
                    "flash_attn",
                    enum_p("Flash attention (--flash-attn).", &["auto", "on", "off"]),
                ),
                (
                    "cache_type_k",
                    enum_p(
                        "KV cache K quantization (--cache-type-k). q8_0 roughly halves KV \
                         memory versus the f16 default.",
                        &["f32", "f16", "bf16", "q8_0", "q5_1", "q5_0", "q4_1", "q4_0", "iq4_nl"],
                    ),
                ),
                (
                    "cache_type_v",
                    enum_p(
                        "KV cache V quantization (--cache-type-v).",
                        &["f32", "f16", "bf16", "q8_0", "q5_1", "q5_0", "q4_1", "q4_0", "iq4_nl"],
                    ),
                ),
                (
                    "cache_ram",
                    int_p(
                        "Host RAM in MiB for holding the prompt state of idle slots \
                         (--cache-ram), so returning to an earlier conversation skips \
                         reprocessing it. -1 = no limit, 0 = disabled; omit to keep \
                         llama.cpp's own default of 8192.",
                    ),
                ),
                (
                    "fit",
                    enum_p(
                        "Let llama-server shrink UNSET arguments to fit device memory \
                         (--fit). It never touches a value set here, so it cannot silently \
                         cap a context length you configured.",
                        &["on", "off"],
                    ),
                ),
                ("fit_ctx", int_p("Floor on the context --fit may choose (--fit-ctx).")),
                // -- chat template & reasoning --
                (
                    "jinja",
                    bool_p(
                        "Use the chat template embedded in the GGUF (--jinja). Required for \
                         any model with a custom template, and for tool calling.",
                    ),
                ),
                (
                    "chat_template_file",
                    str_p(
                        "Override the embedded template (--chat-template-file), relative to \
                         the models directory.",
                    ),
                ),
                (
                    "reasoning_format",
                    enum_p(
                        "How thinking output is parsed back out of the response \
                         (--reasoning-format). 'deepseek' puts it in message.reasoning_content.",
                        &["auto", "none", "deepseek", "deepseek-legacy"],
                    ),
                ),
                (
                    "reasoning",
                    enum_p("Enable thinking (--reasoning).", &["on", "off", "auto"]),
                ),
                (
                    "reasoning_budget",
                    int_p("Thinking token budget (--reasoning-budget). -1 = unrestricted."),
                ),
                (
                    "reasoning_preserve",
                    bool_p(
                        "Keep the thinking trace of EVERY assistant turn in the history \
                         rather than only the last one (--reasoning-preserve). Honoured \
                         only by templates advertising 'supports_preserve_reasoning'; omit \
                         to leave the template's own default in place.",
                    ),
                ),
                (
                    "reasoning_effort",
                    str_p(
                        "Default thinking depth (--reasoning-effort) for a model whose chat \
                         template reads a 'reasoning_effort' variable (Qwen3.8, GPT-OSS, …); \
                         a request that sets reasoning_effort still overrides it. Usual \
                         levels are minimal, low, medium, high, xhigh, max, and 'none' for \
                         templates that use it to skip thinking — but llama-server passes \
                         the value to the template unchecked, so the accepted set belongs to \
                         that template. Omit to leave the template's own default.",
                    ),
                ),
                (
                    "chat_template_kwargs",
                    str_p(
                        "Further chat-template variables, as the JSON object string \
                         --chat-template-kwargs takes, e.g. '{\"preserve_thinking\":true}'. \
                         This is the only route to that flag — one in extra_args is ignored. \
                         A 'reasoning_effort' key here is folded into the field above, since \
                         both reach the same template variable.",
                    ),
                ),
                // -- sampling --
                ("temp", num_p("Sampling temperature (--temp).")),
                ("top_p", num_p("Nucleus sampling (--top-p).")),
                ("top_k", int_p("Top-k sampling (--top-k).")),
                ("min_p", num_p("Minimum probability (--min-p).")),
                ("repeat_penalty", num_p("Repetition penalty (--repeat-penalty).")),
                ("presence_penalty", num_p("Presence penalty (--presence-penalty).")),
                ("seed", int_p("RNG seed (--seed). -1 = random.")),
                // -- speculative decoding --
                (
                    "draft_gguf_path",
                    str_p(
                        "Speculative drafter GGUF (--model-draft), relative to the models \
                         directory. Often shipped alongside the weights in the same repo.",
                    ),
                ),
                (
                    "spec_type",
                    enum_p(
                        "Speculative decoding strategy (--spec-type). It MUST match the \
                         drafter: an MTP-heads GGUF is 'draft-mtp', a 'dflash'-architecture \
                         one is 'draft-dflash'. lmgw__model_inspect reports the right value \
                         for a given file. 'draft-mtp' without draft_gguf_path requires MTP \
                         layers inside the weights themselves, or the model will not load.",
                        &[
                            "none", "draft-simple", "draft-eagle3", "draft-mtp", "draft-dflash",
                            "draft-dspark", "ngram-simple", "ngram-map-k", "ngram-map-k4v",
                            "ngram-mod", "ngram-cache",
                        ],
                    ),
                ),
                (
                    "spec_draft_n_max",
                    int_p("Tokens to draft per step (--spec-draft-n-max)."),
                ),
                (
                    "spec_draft_n_min",
                    int_p("Minimum drafted tokens (--spec-draft-n-min)."),
                ),
                (
                    "spec_draft_ngl",
                    str_p("Drafter GPU layers (--spec-draft-ngl): a number, 'auto' or 'all'."),
                ),
                // -- everything else --
                (
                    "extra_args",
                    str_p(
                        "Escape hatch for llama-server flags with no field above, one option \
                         per line (e.g. '--no-warmup'). Validated against the running build. \
                         A flag that HAS a field above is ignored here — use the field.",
                    ),
                ),
                (
                    "idle_seconds",
                    int_p("Seconds idle before the model unloads and frees VRAM. 0 = never."),
                ),
                ("enabled", bool_p("Serve this model (a disabled model has no container).")),
                (
                    "public",
                    bool_p("Routable by model id without a manual alias."),
                ),
                (
                    "hold_fallback_mode",
                    enum_p(
                        "Where a request against this model goes while GPU hold is active. \
                         'inherit' (default) uses the global hold fallback alias; 'none' \
                         refuses even when a global fallback is set; 'alias' routes to \
                         hold_fallback instead.",
                        &["inherit", "none", "alias"],
                    ),
                ),
                (
                    "hold_fallback",
                    str_p(
                        "Model alias to route to while GPU hold is active, when \
                         hold_fallback_mode is 'alias'. Must resolve to a non-local route.",
                    ),
                ),
                (
                    "capabilities_override",
                    str_p(
                        "Owner-set capability facts merged over what lmgw derives for \
                         /v1/models: a JSON object with optional keys capabilities \
                         (deep-merged; e.g. {\"input_modalities\":[\"text\",\"image\"],\
                         \"reasoning\":{\"kind\":\"levels\",\"levels\":[\"low\",\"high\"]}}), \
                         max_output_tokens, notes (appended). Use it only for facts the GGUF or \
                         the provider catalog does not state; the result is published with \
                         source \"owner\".",
                    ),
                ),
                (
                    "ladder",
                    ladder_p(
                        "Turn this row into a ladder: rungs above the base, given as a JSON \
                         array string, e.g. '[{\"gguf_path\":\"top.gguf\",\"ctx_size\":65536}]' \
                         — each a GGUF and its own --ctx-size (gguf_path/ctx_size here stay the \
                         row's own, as rung 1). Every other field — template, sampling, \
                         projector, drafter, parallel, cache types — is shared by every rung. A \
                         request that outgrows the running rung makes the whole model climb to \
                         the smallest rung that fits, and it only comes back down when the \
                         container stops. Refused at save time unless: n_predict is set and \
                         positive (no rung can promise an answer fits without a max output \
                         ceiling); parallel is at least 1 with no effectively-unified KV cache \
                         (a shared pool has no per-slot guarantee); each rung's per-slot context \
                         (ctx_size / parallel) is strictly increasing, base included, exceeds \
                         n_predict (room for at least one prompt token) and stays at or below its \
                         own GGUF's trained context; every rung's GGUF shares the base's \
                         architecture and tokenizer, and — with a projector configured — its \
                         embedding width (the fit check counts identically whichever rung is \
                         running); spec_type draft-mtp needs either every rung's own GGUF to \
                         carry MTP layers or a drafter (draft_gguf_path); and, with a projector \
                         configured, its per-image token bound is known. Omitted or empty leaves \
                         the ladder unchanged, the same convention every optional field here \
                         follows; '[]' or clear: \"ladder\" is what turns the row back into a \
                         plain one.",
                    ),
                ),
                (
                    "clear",
                    str_p(
                        "Field names to reset to unset, comma-separated (e.g. \
                         'spec_type,draft_gguf_path'). Needed because omitting a field means \
                         'leave unchanged'. 'hold_fallback' resets both hold_fallback_mode and \
                         hold_fallback to inherit/unset. 'ladder' resets the row to not-a-ladder \
                         — the base's own gguf_path/ctx_size are untouched.",
                    ),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__aux_model_set",
            writes: true,
            description:
                "Create, update, delete, enable or disable an AUX model: an embedding model \
                 (served on /v1/embeddings with --embeddings) or a reranker (served on \
                 /v1/rerank with --reranking). This is the class for encoders — a GGUF whose \
                 header declares a pooling_type or a classifier head — which have no LM head \
                 and cannot be chat models. Aux models live in their own models directory \
                 (download with lmgw__hf_add target=aux, list with lmgw__gguf_files \
                 target=aux) and are exposed under the aux prefix (usually 'embed/<model_id>'). \
                 Get kind, pooling and ctx_size from lmgw__local_model_plan target=aux rather \
                 than guessing: the wrong pooling produces vectors that look fine and \
                 retrieve garbage. Update is partial: pass only what changes, and use 'clear' \
                 to unset a field. A model's own container starts on its first request; \
                 lmgw__local_model_test target=aux then proves it embeds (or reranks).",
            props: vec![
                (
                    "action",
                    enum_p(
                        "What to do.",
                        &["create", "update", "delete", "enable", "disable"],
                    ),
                ),
                ("id", int_p("Aux model row id.")),
                (
                    "model_id",
                    str_p(
                        "Client-facing model id (the part after the aux prefix); also \
                         selects the row when id is omitted.",
                    ),
                ),
                (
                    "gguf_path",
                    str_p(
                        "Weights GGUF, relative to the AUX models directory (e.g. \
                         'Qwen/Qwen3-Embedding-4B-GGUF/Qwen3-Embedding-4B-Q4_K_M.gguf'). \
                         Required on create. Files in the chat models directory cannot be \
                         addressed — download into the aux dir with lmgw__hf_add target=aux.",
                    ),
                ),
                (
                    "kind",
                    enum_p(
                        "'embed' (default on create) serves /v1/embeddings; 'rerank' serves \
                         /v1/rerank. lmgw__model_inspect reports which a file is.",
                        &["embed", "rerank"],
                    ),
                ),
                (
                    "pooling",
                    enum_p(
                        "Pooling strategy (--pooling) for an embedding model. Use the value \
                         lmgw__local_model_plan reads from the GGUF header; omit to let \
                         llama-server pick the model's default. Never set on a rerank model \
                         (--reranking selects rank pooling itself, and a second pooling flag \
                         breaks it). Empty string clears.",
                        &["none", "mean", "cls", "last", "rank"],
                    ),
                ),
                (
                    "ctx_size",
                    int_p(
                        "Context length (--ctx-size). Use the model's trained context from \
                         the plan; llama-server truncates longer inputs silently rather \
                         than rejecting them.",
                    ),
                ),
                (
                    "extra_args",
                    str_p(
                        "Escape hatch for llama-server flags with no field above, one option \
                         per line — this is where placement goes: '--n-gpu-layers 0' runs \
                         the model on the CPU (and the VRAM planner then charges it nothing), \
                         '--threads 16' sizes it, '--mmproj /models/<path>' attaches a \
                         projector for a multimodal embedder. --embeddings, --reranking, \
                         --pooling and --ctx-size are rendered from the fields above and \
                         ignored here.",
                    ),
                ),
                (
                    "idle_seconds",
                    int_p("Seconds idle before the model unloads. 0 = never. Default 300."),
                ),
                ("enabled", bool_p("Serve this model (a disabled model has no container).")),
                (
                    "image",
                    str_p(
                        "Podman image override for this model's own container. Empty or \
                         absent leaves it unset; 'clear' reverts to the aux class image.",
                    ),
                ),
                (
                    "extra_run_args",
                    str_p(
                        "'podman run' args override, one per line. Empty or absent leaves it \
                         unset; name it in 'clear' to revert to the class default.",
                    ),
                ),
                (
                    "warm_start",
                    bool_p("Start this model's container when lmgw launches."),
                ),
                (
                    "hold_fallback_mode",
                    enum_p(
                        "Where a request against this model goes while GPU hold is active. \
                         'inherit' (default) means NO fallback for this class — aux models \
                         never inherit the global chat fallback, because a different \
                         embedding model silently corrupts a vector index; 'alias' routes to \
                         hold_fallback instead.",
                        &["inherit", "none", "alias"],
                    ),
                ),
                (
                    "hold_fallback",
                    str_p(
                        "Model alias to route to while GPU hold is active, when \
                         hold_fallback_mode is 'alias'. Must resolve to a non-local route.",
                    ),
                ),
                (
                    "clear",
                    str_p(
                        "Field names to reset to unset, comma-separated: 'extra_args', \
                         'extra_run_args', 'image', 'hold_fallback' (resets both mode and \
                         alias).",
                    ),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__image_model_set",
            writes: true,
            description:
                "Create, update, delete, enable or disable an IMAGE model: a \
                 stable-diffusion.cpp pipeline served by sd-server on \
                 /v1/images/generations (and /v1/images/edits when the row is an edit \
                 pipeline). This is the class for diffusion models — they have no chat \
                 template and no embeddings, so they are neither a chat nor an aux row. \
                 The whole chain, and the short way round it: lmgw__image_recipes -> \
                 lmgw__image_recipe_add key=<key> (it queues every component and hands back \
                 a prefilled row) -> lmgw__hf_downloads until each says 'done' -> THIS TOOL \
                 with the returned row -> lmgw__local_model_test model_id=<id> target=image \
                 to prove it loads and draws. For a pipeline no recipe covers, download the \
                 files yourself with lmgw__hf_add target=image (one call per component — \
                 they live in different repos) and write 'files' by hand. The container starts on the first request; there is no separate \
                 apply for a model that has never run. \
                 'files' is a map of flag key -> path RELATIVE to the image models \
                 directory, one 'key = value' per line: the key is sd-server's long flag \
                 with '-' written as '_' (model, diffusion_model, vae, clip_l, clip_g, \
                 t5xxl, llm, llm_vision, clip_vision, taesd, control_net, ip_adapter, \
                 photo_maker, upscale_model, high_noise_diffusion_model, tokenizer, \
                 lora_model_dir, hires_upscalers_dir, embd_dir, …). EXACTLY ONE of 'model' \
                 (an all-in-one checkpoint) and 'diffusion_model' (a standalone DiT) is \
                 required — everything else hangs off it. 'args' is the same shape for \
                 runtime and default-generation flags (type, offload_to_cpu, diffusion_fa, \
                 vae_tiling, cfg_scale, steps, sampling_method, width, height, …); a key \
                 with no value is a switch. Both maps are checked against the container \
                 image's own --help when the row is saved, so a wrong key is refused by \
                 name (with a suggestion) instead of failing at the first start — \
                 lmgw__llama_flags does the same job for the chat class. \
                 Per-request parameters are NOT row fields: steps, seed, negative prompt, \
                 sampler and LoRA reach a request through sd.cpp's own \
                 <sd_cpp_extra_args>{…}</sd_cpp_extra_args> block inside the prompt. What \
                 goes in 'args' are this row's DEFAULTS.",
            props: vec![
                (
                    "action",
                    enum_p(
                        "What to do.",
                        &["create", "update", "delete", "enable", "disable"],
                    ),
                ),
                ("id", int_p("Image model row id.")),
                (
                    "model_id",
                    str_p(
                        "Client-facing model id (the part after the image prefix, so \
                         'image/<model_id>'); also selects the row when id is omitted.",
                    ),
                ),
                (
                    "files",
                    str_p(
                        "The pipeline's files, one 'key = path' per line, each path \
                         relative to the image models directory — e.g. \
                         'diffusion_model = Tongyi-MAI/Z-Image-Turbo/z-image-turbo-Q4_K.gguf' \
                         then 'vae = Comfy-Org/flux/ae.safetensors' then \
                         'llm = Qwen/Qwen3-4B-Instruct-GGUF/Qwen3-4B-Q4_K_M.gguf'. Required \
                         on create. Replaces the stored map wholesale on update (send the \
                         whole set, not just the line that changed). A leading '/models/' — \
                         the spelling the rendered command line shows — is dropped for you. \
                         A JSON object is accepted too.",
                    ),
                ),
                (
                    "args",
                    str_p(
                        "Runtime and default-generation flags, one 'key = value' per line \
                         — e.g. 'diffusion_fa' (a bare key is a switch) then \
                         'cfg_scale = 1.0' then 'steps = 8' then 'offload_to_cpu'. Same \
                         replace-wholesale rule as 'files'. A JSON object is accepted too.",
                    ),
                ),
                (
                    "modes",
                    str_p(
                        "What this pipeline does, comma-separated: 'img_gen' (the default \
                         when empty), or 'img_gen,vid_gen' for one that also makes video. \
                         This drives task and endpoints on /v1/models before the container \
                         has ever run; it is compared against what the running server \
                         reports and a mismatch becomes a warning, never a silent \
                         correction.",
                    ),
                ),
                (
                    "edit",
                    bool_p(
                        "This pipeline takes reference images (Kontext, Qwen-Image-Edit, \
                         Z-Image-Omni) and may serve /v1/images/edits. Load-bearing, not \
                         descriptive: sd-server does not refuse a reference-image request \
                         against a pipeline that cannot take one, it segfaults, so lmgw \
                         refuses that request itself for every row without this flag.",
                    ),
                ),
                (
                    "enabled",
                    bool_p("Serve this model (a disabled model has no container)."),
                ),
                (
                    "image",
                    str_p(
                        "Podman image override for this model's own container (the class \
                         default is stable-diffusion.cpp's master-cuda tag). Empty or \
                         absent leaves it unset; 'clear' reverts to the class image.",
                    ),
                ),
                (
                    "extra_run_args",
                    str_p(
                        "'podman run' args override, one per line. Empty or absent leaves \
                         it unset; name it in 'clear' to revert to the class default. The \
                         GPU device flags live here — sd-server links libcuda directly and \
                         will not even print its help without the card.",
                    ),
                ),
                (
                    "warm_start",
                    bool_p("Start this model's container when lmgw launches."),
                ),
                (
                    "idle_seconds",
                    int_p(
                        "Seconds idle before the model unloads. 0 = never. Default 300. \
                         Worth keeping low: a loaded pipeline is 7-13 GiB of the card that \
                         nothing else can use.",
                    ),
                ),
                (
                    "hold_fallback_mode",
                    enum_p(
                        "Where a request against this model goes while GPU hold is active. \
                         'inherit' (default) means NO fallback for this class — the global \
                         chat fallback alias is not an image model; 'alias' routes to \
                         hold_fallback instead.",
                        &["inherit", "none", "alias"],
                    ),
                ),
                (
                    "hold_fallback",
                    str_p(
                        "Model alias to route to while GPU hold is active, when \
                         hold_fallback_mode is 'alias'. Must resolve to a non-local route \
                         — a cloud image model.",
                    ),
                ),
                (
                    "capabilities_override",
                    str_p(
                        "Owner override of the derived /v1/models facts for this row, as a \
                         JSON object (keys: capabilities, max_output_tokens, notes). Empty \
                         clears it.",
                    ),
                ),
                (
                    "clear",
                    str_p(
                        "Field names to reset to unset, comma-separated: 'args', 'modes', \
                         'extra_run_args', 'image', 'hold_fallback' (resets both mode and \
                         alias), 'capabilities_override'. Not 'files': a row with no files \
                         names no pipeline.",
                    ),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__local_model_test",
            writes: true,
            description:
                "Load a configured local model and prove it does its job: a chat model \
                 generates one token, an embedding model returns one vector (checked to be \
                 non-zero — an embedder misconfigured as a reranker answers with zeros), a \
                 reranker scores two documents, an image model draws one 256x256 four-step \
                 image and reports how many bytes came back. Works for chat, aux and image \
                 models alike; the probe is chosen by class and kind, so an encoder is never \
                 judged by token generation and a diffusion pipeline is never asked to chat. \
                 The image probe goes out on /v1/images/generations and never on \
                 /v1/images/edits — an edit sends a reference image, which a pipeline that \
                 cannot read one dies on rather than refusing. Starts the model's own container itself (through the same \
                 admission path a real request takes), so there is no separate 'apply first' \
                 step — reports the verbatim llama-server error and the relevant container \
                 log lines when it fails, plus a hint for the known failure modes. Loading a \
                 large model off disk can take minutes and occupies VRAM, which is why this \
                 counts as a mutating tool. For a chat model that loaded successfully, also \
                 compares the running build's /props chat_template_caps and modalities with \
                 what /v1/models derives statically for this row and lists disagreements — an \
                 empty list means the static derivation agrees with the running build; a null \
                 props means /props could not be read, which never fails the test. A chat row \
                 with a ladder tests every rung in turn instead of one: whatever is running is \
                 stopped first (refused, touching nothing, while it is still serving real \
                 requests), the base is loaded and probed, then each higher rung is climbed to \
                 and probed the same way, in order — a failure ends the run there, since nothing \
                 above a broken rung can be reached through it. The result's 'rungs' array \
                 reports each one: rung k/n, the GGUF, its context and per-slot context, how \
                 many seconds it took to become ready, and ok/error. Neither the opening nor the \
                 closing stop is forced, so a real request is never killed for this — but that \
                 means the closing stop can also be refused: an idle claim counts as 'still \
                 serving' too (a tool loop or an agent run sitting between turns), and \
                 reset_to_base: false then means the model is left up on the last rung this test \
                 reached, to be stopped by hand (or lmgw__container action=restart) rather than \
                 at the next request. Any real traffic already on the model when this runs waits \
                 out each rung's drain and reload along with it — several seconds per rung — and \
                 ends up moved to the same top rung, reprocessing its history on its next turn.",
            props: vec![
                ("model_id", str_p("The local model id to load and test.")),
                (
                    "target",
                    enum_p(
                        "The model's class. Omit to find it by id; needed only when two \
                         classes share the same id.",
                        &["chat", "aux", "image"],
                    ),
                ),
            ],
            required: &["model_id"],
        },
    ]
}
