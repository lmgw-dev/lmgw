//! The audio class on the tool plane: `lmgw__audio_catalog` (audio.cpp's
//! package catalog: list, refresh, download), `lmgw__audio_model_set`
//! (audio model rows) and `lmgw__voice_transcribe` (voice-library clip
//! transcripts, audio-class gap 5). Their dispatch is `selfadmin/audio.rs`.

use crate::mcp::selfadmin::{bool_p, enum_p, int_p, str_p, Builtin};

pub(super) fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "lmgw__audio_catalog",
            writes: true,
            description:
                "audio.cpp's model catalog — the way to bring an AUDIO model (speech \
                 recognition, text-to-speech, voice cloning, music, separation, …) onto this \
                 gateway. audio.cpp models are not single GGUFs: a family (pocket_tts, \
                 kokoro, qwen3_asr, …) ships packages (one precision each), and a package \
                 is a set of files in one directory. action=list answers the catalog cached \
                 on this machine — no network call: every family with its tasks, \
                 languages, built-in voices and status, and every package with whether it \
                 is installed (or partly — missing_files names what it lacks, and \
                 incomplete marks a package the spec grew since it was downloaded), \
                 whether an enabled row loads its weights (served, with served_by naming the \
                 rows; serving_unclear when a row's directory holds several packages and no \
                 weight_id picks one, or a GGUF no package ships; a family's served means one \
                 of its packages is, and its serving_note names a row that loads none of \
                 them), its size, \
                 whether its repo is licence-gated, the commit the spec pins it to \
                 (pinned_commit; pin_followed says whether downloads take it — the setting \
                 audio.catalog_revision, 'pinned' by default, else 'latest' = main), which \
                 commit its downloaded files came from (downloaded_from), and the \
                 suggested_model_id, \
                 suggested_path, suggested_task and suggested_mode a row for it takes. \
                 Listing every family leaves out each family's option schema (it is long); \
                 name a family to get its load, session and request options. \
                 action=refresh fetches the catalog from audio.cpp's model_specs on GitHub, \
                 lists each package repo on Hugging Face once (so list names the files a \
                 repo does not publish yet: unpublished_files, availability_note), and keeps \
                 it — call it once when list says nothing is cached. What a refresh could not \
                 do (a spec file, a repo listing) comes back as warnings, which list repeats \
                 until the next refresh; a family whose spec file failed is kept as the \
                 previous refresh had it, and a repo that could not be listed keeps its \
                 previous listing. \
                 action=download queues the files one package lacks (family + package; \
                 all of them for a package never downloaded; under pinned also its installed \
                 files from another commit than the pin whose bytes differ there, so the \
                 package is one commit; one with the same ETag at the pin is recorded at it \
                 instead of fetched again) \
                 through the shared download \
                 queue into the audio models directory, at the revision named in its answer \
                 (the spec's pin, else main); a pin the hub no longer has fails the download \
                 rather than falling back to main. The whole chain: \
                 list -> download -> lmgw__hf_downloads until each queued id says 'done' -> \
                 lmgw__audio_model_set action=create with the package's suggested_* fields \
                 -> a request to /v1/audio/speech or /v1/audio/transcriptions on \
                 'audio/<model_id>' to prove it loads. A gated repo is refused while no \
                 Hugging Face token is set. A mutating tool, because refresh and download \
                 write — hidden at self-admin 'read only' like the others.",
            props: vec![
                (
                    "action",
                    enum_p(
                        "'list' (the cached catalog), 'refresh' (fetch it again from \
                         audio.cpp) or 'download' (queue the files one package lacks).",
                        &["list", "refresh", "download"],
                    ),
                ),
                (
                    "family",
                    str_p(
                        "The family id, e.g. 'kokoro' or 'qwen3_asr'. list: answer this \
                         family only, with its option schema. download: required.",
                    ),
                ),
                (
                    "package",
                    str_p(
                        "download only, required: the package id within that family, as \
                         list names it.",
                    ),
                ),
                (
                    "search",
                    str_p(
                        "list only: case-insensitive substring filter on a family's id, name, \
                         description, tasks and package ids (e.g. 'tts', 'german').",
                    ),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__audio_model_set",
            writes: true,
            description:
                "Create, update, delete, enable or disable an AUDIO model: an audio.cpp \
                 model served in its own container on /v1/audio/speech (tts), \
                 /v1/audio/transcriptions (asr) and audio.cpp's /v1/tasks routes, exposed \
                 as 'audio/<model_id>'. A row names a model DIRECTORY under the audio \
                 models dir (path), its family and its task — take all three from \
                 lmgw__audio_catalog action=list (a package's suggested_path, \
                 suggested_task and suggested_mode; its family's option schema says what \
                 load_options, session_options and default_request_options accept). Saving \
                 checks the task, the path and the voice presets: every preset needs a \
                 voice_id (a voice the family ships) or a voice_ref (a clip under the models \
                 dir, which must exist — audio.cpp opens it at start and exits without it), \
                 and a default_voice_preset that names a preset must name a declared one. \
                 Some TTS families (Pocket) refuse a request without a voice, so set a \
                 default_voice_preset for those. Update is partial: pass only what changes. \
                 Every update stops the model's container so the next request starts it \
                 with the new configuration (a busy one keeps running until it is idle). \
                 'clear: residency' forgets the residency lmgw learned for this row (what its \
                 container holds on the GPU once loaded), so it is charged its on-disk size \
                 again until a request teaches it anew. backend='cpu' runs the row on the \
                 CPU: no VRAM charged, never evicted, not stopped by the GPU hold (it keeps \
                 serving), and no residency learned; threads sets its thread count (default: \
                 this machine's physical cores for a row switched to the CPU, the audio class's \
                 threads for every other row). After create there is no separate \
                 apply: the container starts on the first request — prove the row with a \
                 request to /v1/audio/speech or /v1/audio/transcriptions on 'audio/<model_id>', \
                 read it back with lmgw__local_model_get target=audio, and see a start that \
                 failed with lmgw__container model=<model_id> action=logs.",
            props: vec![
                (
                    "action",
                    enum_p(
                        "What to do.",
                        &["create", "update", "delete", "enable", "disable"],
                    ),
                ),
                ("id", int_p("Audio model row id.")),
                (
                    "model_id",
                    str_p(
                        "Client-facing model id (the part after the audio prefix, so \
                         'audio/<model_id>'). Required on create. Without id it selects the \
                         row to update, delete, enable or disable; WITH id, on update, it \
                         renames the row, and the old container is removed.",
                    ),
                ),
                (
                    "family",
                    str_p(
                        "audio.cpp model family, e.g. 'pocket_tts' or 'qwen3_asr' — the \
                         catalog's family id. Required on create.",
                    ),
                ),
                (
                    "path",
                    str_p(
                        "The model directory, RELATIVE to the audio models dir — a \
                         package's suggested_path, e.g. \
                         'audio-cpp/audio.cpp-gguf/PocketTTS-GGUF/german'. Required on \
                         create.",
                    ),
                ),
                (
                    "task",
                    enum_p(
                        "What the model does: tts, asr, gen (music, sound), clon (voice \
                         cloning), vc, svc, s2s, sep (separation), vad, diar, align, vdes \
                         (voice design), spk, midi. A package's suggested_task. Required on \
                         create.",
                        &[
                            "tts", "asr", "gen", "clon", "vc", "svc", "s2s", "sep", "vad",
                            "diar", "align", "vdes", "spk", "midi",
                        ],
                    ),
                ),
                (
                    "mode",
                    enum_p(
                        "'offline' (default: one buffered answer) or 'streaming' (the \
                         family streams its output as server-sent events).",
                        &["offline", "streaming"],
                    ),
                ),
                (
                    "load_options",
                    str_p(
                        "audio.cpp load options as a JSON object (written as a JSON string \
                         on this tool plane), e.g. '{\"language\": \"de\"}' — the family's \
                         'load' options. Replaces the stored object; '{}' empties it.",
                    ),
                ),
                (
                    "session_options",
                    str_p(
                        "audio.cpp session options as a JSON object (a JSON string here), \
                         e.g. '{\"weight_type\": \"q8_0\"}' — the family's 'session' \
                         options. Replaces the stored object; '{}' empties it.",
                    ),
                ),
                (
                    "default_request_options",
                    str_p(
                        "Request-option defaults applied to every call, as a JSON object (a \
                         JSON string here), e.g. '{\"speed\": 1.1}' — the family's 'request' \
                         options. A request that names one still wins.",
                    ),
                ),
                (
                    "lazy",
                    bool_p(
                        "Load the model on its first request (true) or at container start \
                         (false). Omit to inherit the audio class setting; name 'lazy' in \
                         clear to go back to inheriting.",
                    ),
                ),
                (
                    "busy_timeout_ms",
                    int_p(
                        "How long a request waits for the model while another one runs, in \
                         ms; 0 waits forever. Negative is refused. Name 'busy_timeout_ms' in \
                         clear to inherit again.",
                    ),
                ),
                (
                    "backend",
                    enum_p(
                        "Where this row runs: 'cpu' runs it on the CPU — no VRAM charged, \
                         never evicted, not stopped by the GPU hold, no residency learned; \
                         its inherited run args lose the GPU passthrough. Omit to keep, name \
                         'backend' in clear to inherit the audio class backend again. No \
                         other value: another GPU backend is the class setting.",
                        &["cpu"],
                    ),
                ),
                (
                    "threads",
                    int_p(
                        "This row's thread count, 1 or more (no upper cap; above this \
                         machine's CPUs the save says so). Name 'threads' in clear for the \
                         default: this machine's physical cores for a row switched to the CPU \
                         (backend='cpu' on the row), the audio class's threads for every other \
                         row — one that inherits a class backend of cpu included. Several CPU \
                         rows busy at once each run their own pool, so split the cores between \
                         them.",
                    ),
                ),
                (
                    "model_spec_override",
                    str_p(
                        "A '<family>.json' spec (or a directory of them) relative to the \
                         audio models dir that replaces the image's built-in catalog for \
                         this row. Empty clears.",
                    ),
                ),
                (
                    "config_id",
                    str_p("Named config asset id, for a model directory holding several. Empty clears."),
                ),
                (
                    "weight_id",
                    str_p(
                        "Named weights asset id, for a model directory holding several \
                         variants (e.g. 'q8_0'). Empty clears.",
                    ),
                ),
                (
                    "voice_presets",
                    str_p(
                        "Named voices as a JSON object (a JSON string here): name -> \
                         {\"voice_id\": \"<shipped voice>\"} or {\"voice_ref\": \
                         \"/models/<clip>.wav\"}, e.g. '{\"alba\": {\"voice_id\": \
                         \"alba\"}}'. Replaces the stored object.",
                    ),
                ),
                (
                    "default_voice_preset",
                    str_p(
                        "The voice a request that names none gets: a preset name from \
                         voice_presets, or an inline preset as a JSON object string. Empty \
                         clears.",
                    ),
                ),
                ("enabled", bool_p("Serve this model (a disabled model has no container).")),
                (
                    "image",
                    str_p(
                        "Podman image override for this model's own container. Empty \
                         clears back to the audio class image.",
                    ),
                ),
                (
                    "extra_run_args",
                    str_p(
                        "'podman run' args override, as shell-quoted text (one option per \
                         line is fine). Empty or absent leaves it unchanged; name it in \
                         'clear' to revert to the class default — which carries the GPU and \
                         SELinux flags, so override it only knowingly. An override with no \
                         args is never stored: it is the class default. A row on the CPU runs \
                         its own override as written; one that still passes a GPU is named in \
                         lmgw__local_model_get's problems.",
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
                         'inherit' (default) means NO fallback for this class; 'alias' \
                         routes to hold_fallback instead.",
                        &["inherit", "none", "alias"],
                    ),
                ),
                (
                    "hold_fallback",
                    str_p(
                        "Model alias to route to while GPU hold is active, when \
                         hold_fallback_mode is 'alias' — a cloud audio model. Unused on a \
                         row on the CPU, which the hold does not stop.",
                    ),
                ),
                (
                    "clear",
                    str_p(
                        "Field names to reset, comma- or space-separated: 'lazy', \
                         'busy_timeout_ms', 'backend', 'threads', 'extra_run_args', \
                         'hold_fallback' (resets both mode and alias), and on update \
                         'residency' (forget the learned \
                         residency). A field named here is unset on create too, whatever \
                         value came with it.",
                    ),
                ),
            ],
            required: &["action"],
        },
        Builtin {
            name: "lmgw__voice_transcribe",
            writes: true,
            description:
                "Write voice-library clip transcripts with a LOCAL speech-to-text model. A \
                 cloning text-to-speech model that takes reference_text (Fish, CosyVoice3) \
                 clones a library clip well only with its transcript, which audio.cpp hands it \
                 when a request's voice names the clip; GET /v1/audio/voices shows which clips \
                 have one (transcript: true|false). Without clip, every clip lacking a \
                 transcript is transcribed; with clip, that one (its transcript is replaced). \
                 The model is alias, else the setting audio.voice_transcribe_alias. The clips \
                 are the owner's voice, so only a local speech-to-text (asr) audio model may \
                 hear them: a cloud alias is refused, and under the GPU hold the call is \
                 refused rather than sent to a fallback. The answer names the clips \
                 transcribed and their lengths, never the text (the Audio lab shows it).",
            props: vec![
                (
                    "clip",
                    str_p(
                        "One clip: its file name (me.wav) or voice name (me). Empty: every \
                         clip without a transcript.",
                    ),
                ),
                (
                    "alias",
                    str_p(
                        "The local speech-to-text model, e.g. 'audio/qwen3-asr'. Empty: the \
                         setting audio.voice_transcribe_alias.",
                    ),
                ),
            ],
            required: &[],
        },
    ]
}
