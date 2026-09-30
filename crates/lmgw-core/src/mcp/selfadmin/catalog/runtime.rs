//! Container and gateway-settings mutations: `lmgw__container`,
//! `lmgw__hold_set`, `lmgw__settings_set`.

use crate::mcp::selfadmin::{bool_p, enum_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__container",
            writes: true,
            description:
                "Control the per-model container runtime: lmgw runs one podman container per \
                 loaded chat/aux/audio/image model, started on demand and stopped \
                 independently — there is no shared per-class container any more. Address one \
                 model with `model=<id>` (its class is found automatically from the \
                 local/aux/audio/image tables; pass `target` too only to disambiguate an id that exists in more than \
                 one class); address a whole group with `target` alone. 'start' on a model \
                 warms it (a no-op if it is already up); 'start' on a group only starts that \
                 group's warm_start-flagged models, never every configured one — starting an \
                 entire class at once can ask for more VRAM than the box has, so start any \
                 other model on demand with `model=<id>`. 'stop' refuses a model still serving \
                 requests unless `override=true`. 'restart' and 'apply' are the same operation \
                 on a single model (stop, then start with freshly rendered arguments — there \
                 is no drift concept, argv is always rendered fresh); on a group, 'apply' \
                 additionally reports the same pre-flight lmgw__local_model_check runs (missing \
                 files, spec_type mismatches) for every enabled model in scope, not only the \
                 running ones. 'logs' (model only) returns the container's recent output — \
                 with one container per model this is the only place a failed start is visible.",
            props: vec![
                (
                    "target",
                    enum_p(
                        "Which group of models. Omit when 'model' is given. 'embed' is \
                         accepted as the old name for 'aux'.",
                        &["all", "chat", "aux", "audio", "image"],
                    ),
                ),
                (
                    "model",
                    str_p(
                        "Address one model by its id instead of a whole group.",
                    ),
                ),
                (
                    "action",
                    enum_p(
                        "What to do. 'logs' requires 'model'.",
                        &["status", "start", "stop", "restart", "apply", "logs"],
                    ),
                ),
                (
                    "override",
                    bool_p(
                        "Force a 'stop' that would otherwise be refused because the model is \
                         still serving requests.",
                    ),
                ),
                (
                    "tail",
                    int_p("Lines of container output 'logs' returns, from the end. Default 60."),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__hold_set",
            writes: true,
            description:
                "Engage or release lmgw's GPU hold. While the hold is on, lmgw stays off the \
                 GPU without going down: every request that would need a local container is \
                 either re-routed to that model's configured fallback alias (a cloud model) or \
                 refused with a 503 whose error code is 'gpu_hold' — cloud routes are \
                 unaffected. Engaging it also stops every resident local container that is \
                 idle right now, and refuses every new local load: no request start, no warm \
                 start, no lmgw__container start/restart, no lmgw__local_model_test. Models \
                 that are mid-request are never killed — they come back as 'draining' and are \
                 stopped as soon as they finish. Releasing it puts everything back: models \
                 start on demand again. Engaging it also aborts a benchmark run that is going \
                 (lmgw__bench_start), removing its container. The switch is persisted, so it \
                 survives a gateway restart. Read the current state from lmgw__status (vram.hold_active, \
                 vram.draining) or lmgw__settings (hold); set the global chat fallback alias \
                 with lmgw__settings_set hold_fallback_alias, and a per-model one with \
                 lmgw__local_model_set.",
            props: vec![(
                "active",
                bool_p("true engages the hold (and stops idle local containers); false \
                        releases it."),
            )],
            required: &["active"],
        },
        Builtin {
            name: "lmgw__settings_set",
            writes: true,
            description:
                "Change gateway settings. Pass only the fields to change. Deliberately \
                 excluded: the self-admin mode itself, the bind address, the HF/update/forge \
                 tokens and the builds directory — change those in the dashboard.",
            props: vec![
                (
                    "auth_enabled",
                    bool_p("Require a gateway API key on /v1/* and /mcp."),
                ),
                ("retention_days", int_p("Days of request logs to keep. 0 = forever.")),
                (
                    "retention_max_rows",
                    int_p("Hard row cap on the request log. 0 = unlimited."),
                ),
                (
                    "max_body_mb",
                    int_p(
                        "Largest request body accepted on the JSON /v1 routes, in MiB. \
                         0 = unlimited. Raise it when clients inline large base64 images \
                         or documents; over it they get a 413 with code 'body_limit'. \
                         Applies to the next request, no restart. /v1/audio/* and \
                         /v1/tasks/* are unbounded regardless.",
                    ),
                ),
                (
                    "chat_archive_days",
                    int_p(
                        "Archive a Chat thread this many days after its last activity \
                         (a new message or a settings change; pinning does not count). \
                         0 disables auto-archive. Archiving is reversible: a pin, a \
                         restore, or sending into the thread brings it back.",
                    ),
                ),
                (
                    "chat_purge_days",
                    int_p(
                        "Delete an archived, unpinned Chat thread this many days after it \
                         was archived. 0 keeps archived threads forever. The clock is when \
                         it was archived, not when it was last active, so a thread archived \
                         by hand gets the full period too.",
                    ),
                ),
                (
                    "chat_pdf_mode",
                    enum_p(
                        "How a text PDF attached in Chat starts out: 'text' (the extracted \
                         text, default), 'images' (every page as an image; needs a vision \
                         model) or 'ask' (the chip starts unset and Send waits for a choice).",
                        &crate::config::CHAT_PDF_MODES,
                    ),
                ),
                (
                    "chat_stt_alias",
                    str_p(
                        "Speech-to-text alias that transcribes a Chat audio attachment when \
                         the thread's model takes no audio itself. Must be a model whose \
                         capability task is 'asr'. '' = none (Send is then blocked for such \
                         a thread).",
                    ),
                ),
                (
                    "chat_kb_budget_tokens",
                    int_p(
                        "Tokens of knowledge-base excerpts one Chat turn may carry; a thread \
                         can override it. Must be above 0 (default 4000).",
                    ),
                ),
                (
                    "chat_system_prompt",
                    str_p(
                        "The system prompt a new Chat thread starts with; each thread keeps \
                         its own copy, so existing threads are not changed. {{model}} and \
                         {{date}} in a thread's prompt become its model alias and today's \
                         date when a message is sent. Passing the built-in text (see \
                         lmgw__settings chat_system_prompt while chat_system_prompt_is_builtin \
                         is true) returns to the built-in default; '' starts new threads with \
                         no system prompt.",
                    ),
                ),
                (
                    "sampling_alias",
                    str_p("Default model alias answering MCP sampling requests."),
                ),
                (
                    "update_check_enabled",
                    bool_p("Poll the package registry for new lmgw releases."),
                ),
                (
                    "hold_fallback_alias",
                    str_p(
                        "Global GPU-hold fallback: the alias a held chat-class local model \
                         falls back to when its own row's hold_fallback_mode (on \
                         lmgw__local_model_set) is 'inherit'. Must resolve to a non-local \
                         route; '' clears it back to 'refuse'. The hold switch itself is not \
                         settable here — engaging it stops containers, a side effect this \
                         generic settings call must not carry.",
                    ),
                ),
                (
                    "fallback_on_external",
                    bool_p(
                        "Fall back when VRAM outside lmgw's control is short \
                         (vram.fallback_on_external): when a local model does not fit \
                         because games, browsers or other apps hold GPU memory lmgw cannot \
                         free, its configured fallback answers at once instead of queueing. \
                         Contention between lmgw's own models still queues. On by default; \
                         turn off on shared-memory systems (APUs), where GPU memory is host \
                         RAM that grows and shrinks with everything else running.",
                    ),
                ),
                (
                    "build_update_check_hours",
                    int_p(
                        "How often lmgw checks whether a container build's branch or PRs \
                         have moved since its last verified run, in hours: 0 to 8760 (a \
                         year). 0 turns the periodic check off (a check on demand still \
                         works). Default 6.",
                    ),
                ),
            ],
            required: &[],
        },
    ]
}
