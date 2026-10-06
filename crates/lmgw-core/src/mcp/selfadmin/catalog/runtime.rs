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
                 with one container per model this is the only place a failed start is visible. \
                 'status' shows, for a llama-server container, what it said about itself in \
                 GET /props at its start (llama_props: build_info, modalities, n_ctx_slot — \
                 null where the server did not say), or among its warnings why it could not.",
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
                 stopped as soon as they finish. Audio models that run on the CPU (an \
                 audio row with backend 'cpu') keep serving: they use no VRAM, so the hold \
                 neither stops nor refuses them (named as kept_on_cpu). Releasing it puts \
                 everything back: models start on demand again. Engaging it also aborts a benchmark run that is going \
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
                        "The Chat's speech-to-text alias: dictation, voice mode (a realtime \
                         session bound to the thread), and the transcript of an audio \
                         attachment when the thread's model takes no audio itself. Must be a \
                         model whose capability task is 'asr'. '' = realtime.asr_alias; \
                         realtime falls back the other way (a /v1/realtime session uses this \
                         alias while realtime.asr_alias is empty), so either one serves both. \
                         A thread's own override wins over both. With none of the three set \
                         the thread cannot transcribe: dictation and voice mode are refused \
                         (asr_not_configured), and an audio attachment its model cannot hear \
                         blocks Send. The audio goes where the alias routes, its GPU-hold \
                         fallback included; the model that answered is named.",
                    ),
                ),
                (
                    "chat_tts_alias",
                    str_p(
                        "The Chat's text-to-speech alias, for read-aloud and voice mode. Must \
                         be a model whose capability task is 'tts' or 'vdes'. '' = \
                         realtime.tts_alias (realtime never falls back to this one). A thread \
                         can override it. It loads on first use (a speaker press, a turn read \
                         aloud, entering voice mode), never when a chat opens.",
                    ),
                ),
                (
                    "chat_voice",
                    str_p(
                        "The Chat's voice, a voice of its text-to-speech model \
                         (chat_tts_alias, else realtime.tts_alias). '' = none named: realtime's \
                         voice chain decides (realtime.default_voice, then the TTS row's \
                         default preset). Not checked on save: a name the model does not know \
                         fails the speech with voice_not_found. A thread can override it; a \
                         thread that speaks with another model than the Chat's does not take \
                         it. Each thread also keeps its own TTS seed, so a model that designs \
                         or draws its voice keeps one voice per thread.",
                    ),
                ),
                (
                    "chat_speech_style",
                    str_p(
                        "The speech instructions the Chat's text-to-speech model gets (a \
                         speaking style, or a voice description for a voice-design model). \
                         '' = realtime.speech_instructions. Owner-wide like that setting, it \
                         stands back for a voice-design row that describes its own voice. A \
                         thread can override it, '' included (no style for that thread), and a \
                         thread's own style wins over the row's description.",
                    ),
                ),
                (
                    "chat_voice_language",
                    str_p(
                        "The language the user speaks in the Chat, an ISO 639-1 code such as \
                         'de' or 'en' (case folded; anything else is refused): the \
                         speech-to-text model is told it where it takes a language, and the \
                         prompt says the user speaks it. Replies are in \
                         chat_voice_reply_language, which falls back to this one. '' = none: \
                         the ASR detects, and without a reply language the reply follows the \
                         user's language; it does not fall back to a realtime setting. A thread \
                         can override it (voice.language).",
                    ),
                ),
                (
                    "chat_voice_reply_language",
                    str_p(
                        "The language Chat replies are in, an ISO 639-1 code such as 'en' (case \
                         folded; anything else is refused): the model is asked to answer in it, \
                         and the text-to-speech model speaks it where it takes a language (a \
                         thread's voice_resolved.language_notes say where a model does not). \
                         '' = the language the user speaks (chat_voice_language), so one \
                         language set there drives both. Set both to speak one language and \
                         hear replies in another. It also picks the words read-aloud and voice \
                         mode announce a skipped code block or table with (en, de, fr, es, it; \
                         English otherwise). A thread can override it (voice.reply_language).",
                    ),
                ),
                (
                    "chat_read_aloud",
                    bool_p(
                        "Read Chat replies aloud as they stream (default false): the page that \
                         sends a turn (send, edit, regenerate, continue) asks for its speech, \
                         which starts at the first clause while the text still streams. \
                         Tables and fenced code are not read; each is announced ('Code block, \
                         rust.', 'Table.'). A thread can override it; voice mode speaks on its \
                         own and ignores it.",
                    ),
                ),
                (
                    "chat_turn_detection",
                    enum_p(
                        "How voice mode in the Chat detects the end of a turn: 'semantic_vad' \
                         (Silero VAD plus Smart Turn, default), 'server_vad' (silence only) or \
                         'push_to_talk' (hold Space or the Talk button). It is the mode the \
                         panel starts in; the panel's Auto / Push to talk switch changes it for \
                         that session only (a thread at push_to_talk switched to Auto uses \
                         semantic_vad). A thread can override it.",
                        &crate::store::TurnDetection::NAMES,
                    ),
                ),
                (
                    "chat_voice_audio_input",
                    enum_p(
                        "Whether a voice-mode turn goes to the chat model as audio: 'off' \
                         (default: the model reads the speech-to-text transcript) or 'on' \
                         (experimental). Under 'on' a turn goes as audio when the model that \
                         answers it — the thread's model, or the fallback a GPU hold, a \
                         benchmark run, an outside-VRAM verdict or a candidate alias's walk \
                         hands it to — is a chat model whose input_modalities include audio \
                         and lmgw can send it audio (OpenAI-compatible, llama.cpp and Gemini \
                         upstreams; not Anthropic), without a context guard (a ladder or a \
                         guarded shared KV pool), and the thread's knowledge bases are not in \
                         auto mode; otherwise it goes as the transcript, and the thread's \
                         voice_resolved.audio_input says why. A model whose capabilities lmgw \
                         cannot read counts as not taking audio: to make it hear, give its \
                         alias (or local row) a capabilities override with task chat and \
                         input_modalities text and audio — a passthrough model needs an alias \
                         for that. A configured fallback is \
                         always used, wherever it runs, and hears the turn when it takes \
                         audio. The speech-to-text \
                         model still transcribes every turn — with either value, so a cloud \
                         speech-to-text model still receives each turn's audio: the transcript \
                         is what is stored (no audio is), and the reply is held until it is \
                         in. Dictation stays on speech-to-text. A thread can override it.",
                        &crate::store::AudioInputMode::NAMES,
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
                (
                    "audio_catalog_revision",
                    enum_p(
                        "What an audio catalog download (lmgw__audio_catalog action=download) \
                         takes when the audio.cpp spec pins a package to a commit: 'pinned' \
                         (default) fetches every file of the package at that commit, the one \
                         the spec was tested with; 'latest' always takes main. A package the \
                         spec does not pin takes main either way. lmgw__hf_set check_updates \
                         and redownload follow the same rule for catalog files, at the pin the \
                         spec names now. A pin that cannot be fetched fails the download — lmgw \
                         never falls back to main on its own. Read back as \
                         audio.catalog_revision in lmgw__settings.",
                        &crate::config::CatalogRevision::ALL,
                    ),
                ),
                (
                    "realtime",
                    str_p(
                        "GET /v1/realtime's settings, as a JSON-encoded object with only the \
                         fields to change, named as lmgw__settings reports them under \
                         'realtime' (its two default_instructions_* read-outs are not \
                         settable; lists and maps are replaced whole). The cascade: \
                         default_model (a \
                         chat alias), asr_alias (task asr; '' = the Chat's transcription \
                         model), tts_alias (task tts or vdes), default_voice, model_map and \
                         voice_map ({client name: alias or voice}). Speech: speech_instructions \
                         (the TTS's style, or a voice-design row's description, while a \
                         session sends none; '' = none), tag_hint (an audio response's prompt \
                         says what square brackets do: the sounds the TTS makes, or delivery \
                         cues such as [laughing] for a style or passthrough TTS that renders \
                         no tags, e.g. Qwen3 CustomVoice; a cloud TTS only when its alias \
                         declares capabilities.speech.instructions 'style'). \
                         default_instructions: the \
                         built-in text (default_instructions_builtin) returns to the built-in \
                         prompt, '' turns it off. Turn detection: threshold, prefix_padding_ms, \
                         silence_duration_ms, semantic_vad_engine ('smart_turn' | \
                         'server_vad'), semantic_vad ({high|medium|low: {threshold, floor, \
                         max_wait_ms, silence_duration_ms}}, a row or field left out keeps \
                         its value), semantic_floor_window_ms. Barge-in: barge_in_min_ms, \
                         barge_in_guard_ms, post_interrupt_silence_ms, half_duplex, \
                         echo_tail_ms, barge_in_check ('words' | 'duration'), \
                         backchannel_words, barge_in_check_scripts, \
                         barge_in_check_timeout_ms, barge_in_check_alias (task asr; '' = the \
                         session's own). Output: output_lead_ms, synthesis_ahead_s, \
                         longest_pause_ms (0 = keep the engine's silences), \
                         warm_on_connect. Limits: max_message_mb, max_frame_mb (not both 0), \
                         ping_interval_s. Refused, naming the field: an alias of the wrong \
                         task, a semantic_vad row that cannot run, an unknown script. \
                         Example: '{\"tts_alias\": \"audio/pocket-tts\", \
                         \"warm_on_connect\": true}'.",
                    ),
                ),
            ],
            required: &[],
        },
    ]
}
