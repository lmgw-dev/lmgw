//! The plain-language half of a model's `/v1/models` entry (design §2.1,
//! `notes` bullet).
//!
//! One rule governs everything here: **every sentence must be true for the row
//! it is attached to**. Generic boilerplate ("local models start on first
//! request", "a 503 gpu_hold means …") belongs to the list-level `lmgw.notes`
//! block, not here, so a note in this module always branches on the row's own
//! facts — its `--reasoning-format`, its reasoning kind, its projector, its
//! `--n-predict` — and is dropped when the fact is unknown.
//!
//! Notes are sentences, not markup: no markdown, no bullets, one or two
//! sentences each, so an agent can put them straight in front of a model and a
//! human can read them in a table cell.

use crate::catalog::ModelInfo;
use crate::config::{AudioModel, AuxKind, AuxModel, ImageModel, LlamaParams, LocalModel, Protocol};
use crate::gguf::{ModelSummary, TemplateSignals};
use crate::runtime::image::ImageCapabilities;

use super::{ModelCapabilities, ProjectorStatus, ReasoningCaps};

// ---------------------------------------------------------------------------
// Local llama.cpp chat rows
// ---------------------------------------------------------------------------

/// Notes for a local chat row. `caps` is the object [`super::for_local_row`]
/// just built — `None` when the GGUF could not be read, which is itself the
/// first thing the notes say. `signals` are the template heuristics behind it,
/// for the facts the published object deliberately leaves out (which effort
/// variable the template reads at all).
pub fn notes_for_local(
    model: &LocalModel,
    caps: Option<&ModelCapabilities>,
    signals: Option<&TemplateSignals>,
    max_output_tokens: Option<u64>,
    projector: ProjectorStatus<'_>,
    projector_summary: Option<&ModelSummary>,
) -> Vec<String> {
    let mut out = Vec::new();
    let p = &model.params;

    let Some(caps) = caps else {
        out.push(format!(
            "The model file {} could not be read, so nothing about this model's \
             template, reasoning or modalities could be derived; only the row's own \
             settings are published.",
            model.gguf_path
        ));
        out.extend(projector_notes(projector, None, p.no_mmproj));
        out.push(max_output_note(max_output_tokens));
        out.extend(ladder_note(model));
        out.extend(pool_guard_note(p));
        return out;
    };

    if let Some(r) = caps.reasoning.as_ref() {
        out.extend(local_reasoning_notes(r, p, signals));
        if let Some(note) = unreachable_effort_note(signals) {
            out.push(note);
        }
        if may_think(r) {
            out.push(trace_note(p.reasoning_format.as_deref()));
        }
    }

    if let Some(m) = caps.input_modalities.as_ref() {
        if m.iter().any(|x| x == "image") {
            out.push(format!(
                "Images: send an OpenAI image_url content part (an https URL or a data: URI) \
                 or an Anthropic image block.{}",
                match projector {
                    ProjectorStatus::Configured { path } => format!(" Projector: {path}."),
                    _ => String::new(),
                }
            ));
        }
        if m.iter().any(|x| x == "audio") {
            out.push(
                "Audio: send OpenAI input_audio parts {data: base64, format: wav|mp3|flac}; \
                 llama.cpp sniffs the actual format itself, so the format field only has to \
                 be present."
                    .to_string(),
            );
        }
    }
    out.extend(projector_notes(projector, projector_summary, p.no_mmproj));

    if let Some(t) = caps.tool_calls.as_ref() {
        match t.kind.as_str() {
            "native" => {
                let syntax = match t.format.as_deref() {
                    Some("unknown") | None => "this model's native tool-call syntax".to_string(),
                    Some(f) => format!("this model's own {f} tool-call syntax"),
                };
                out.push(format!(
                    "Tools: send OpenAI tools/tool_choice or Anthropic tools; llama-server parses \
                     {syntax} into structured tool_calls. Never hand-format a call in the prompt."
                ));
            }
            // §3.4 / review F2: the template renders tools, but in a syntax
            // none of the known markers match — so there is no parser for it.
            "text" => out.push(
                "Tools: this model's chat template renders tools in a syntax lmgw does not \
                 recognise; llama-server may return calls as plain text in message.content \
                 instead of structured tool_calls, so a caller has to parse them itself."
                    .to_string(),
            ),
            _ => out.push(
                "Tools: this model's chat template has no native tool rendering; llama-server \
                 falls back to its generic prompt-injected tool format, which is unreliable — \
                 prefer a model whose tool_calls.kind is native."
                    .to_string(),
            ),
        }
    }

    out.push(max_output_note(max_output_tokens));
    out.extend(ladder_note(model));
    out.extend(pool_guard_note(p));
    out
}

/// Whether a trace can appear at all — a `fixed` model that does not think
/// never produces one, so the "where the trace comes back" sentence would be
/// false for it.
fn may_think(r: &ReasoningCaps) -> bool {
    r.kind != "fixed" || r.enabled == Some(true)
}

/// The reasoning sentence(s) for a local row: the default state and the flags
/// that set it, then how a request changes it (design §2.1, §3.2).
fn local_reasoning_notes(
    r: &ReasoningCaps,
    p: &LlamaParams,
    signals: Option<&TemplateSignals>,
) -> Vec<String> {
    let mut out = Vec::new();
    let state = if r.enabled == Some(true) { "ON" } else { "OFF" };
    let flags = configured_flags(p);
    let flags_clause = flags.map(|f| format!(" ({f})")).unwrap_or_default();

    match r.kind.as_str() {
        "levels" => {
            let at = r
                .default
                .as_deref()
                .map(|d| format!(" at effort '{d}'"))
                .unwrap_or_default();
            let off = if r.can_disable == Some(true) {
                "; x-lmgw-reasoning: off (or reasoning_effort: \"none\") turns it off"
            } else {
                ""
            };
            out.push(format!(
                "Reasoning is {state} by default{at}{flags_clause}. Per request: header \
                 x-lmgw-reasoning-effort: {}, or body reasoning_effort{off}.",
                r.levels.join("|")
            ));
        }
        "toggle" if r.can_disable == Some(true) => out.push(format!(
            "Reasoning is {state} by default{flags_clause}. Per request: header \
             x-lmgw-reasoning: on|off, or body chat_template_kwargs \
             {{\"enable_thinking\": true|false}}."
        )),
        // A toggle without `enable_thinking`: the only variable a request
        // reaches is the effort one, which this template reads (that is why it
        // is a toggle at all) — so the note names that and nothing else.
        "toggle" if signals.is_some_and(super::reads_settable_effort) => out.push(format!(
            "Reasoning is {state} by default{flags_clause}. Per request: body reasoning_effort \
             (or header x-lmgw-reasoning-effort) reaches this template's own reasoning_effort \
             variable."
        )),
        "toggle" => out.push(format!("Reasoning is {state} by default{flags_clause}.")),
        // `fixed` with a thinking template: the row either always thinks, or
        // has had its only brake (a zero budget) pulled.
        _ if r.enabled == Some(true) => out.push(format!(
            "This model always thinks{flags_clause}: its chat template renders the trace \
             unconditionally and reads no switch lmgw can set, so no per-request control turns \
             it off — only --reasoning-budget 0 on the model row does."
        )),
        _ if p.reasoning_budget == Some(0) => out.push(
            "Thinking is switched off on this row by --reasoning-budget 0, and this chat \
             template offers no per-request switch to turn it back on."
                .to_string(),
        ),
        _ => out.push(
            "This model does not reason: its chat template renders no thinking trace and \
             reads no thinking variable, so the reasoning headers do nothing on it."
                .to_string(),
        ),
    }

    if r.kind != "fixed" && r.can_disable == Some(false) {
        out.push(
            "Switching thinking off per request is not possible on this model: its chat \
             template does not read enable_thinking, which is the only switch llama-server \
             has to set."
                .to_string(),
        );
    }
    out
}

/// An effort variable this template reads that no lmgw control can reach
/// (design §2.1 `control`; review F1). Absent when a request's
/// `reasoning_effort` does land somewhere — then the effort controls are
/// published and the reasoning note already describes them.
fn unreachable_effort_note(signals: Option<&TemplateSignals>) -> Option<String> {
    let s = signals?;
    if s.effort_var_names.is_empty() || super::reads_settable_effort(s) {
        return None;
    }
    Some(format!(
        "This model's chat template reads {}, which lmgw does not set: no effort control \
         reaches it, so it thinks at whatever its own template decides.",
        s.effort_var_names.join(" and ")
    ))
}

/// The row's own reasoning flags, verbatim, so the stated default is
/// traceable to a setting rather than asserted.
fn configured_flags(p: &LlamaParams) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = p.reasoning.as_deref() {
        parts.push(format!("--reasoning {v}"));
    }
    if let Some(v) = p.reasoning_effort.as_deref() {
        parts.push(format!("--reasoning-effort {v}"));
    }
    if let Some(v) = p.reasoning_budget {
        parts.push(format!("--reasoning-budget {v}"));
    }
    // Both template kwargs that reach a reasoning variable — `preserve_thinking`
    // is where `preserve_history` can come from, and an unattributed boolean in
    // the published object is exactly what this clause exists to prevent.
    for key in ["enable_thinking", "preserve_thinking"] {
        if let Some(v) = p
            .chat_template_kwargs
            .get(key)
            .and_then(serde_json::Value::as_bool)
        {
            parts.push(format!("chat_template_kwargs.{key} {v}"));
        }
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Where the reasoning trace surfaces, branched on `--reasoning-format`
/// (design §2.1, notes bullet).
fn trace_note(reasoning_format: Option<&str>) -> String {
    match reasoning_format {
        Some("none") => "The reasoning trace is not separated out on this row \
                         (--reasoning-format none): the thoughts stay inside message.content."
            .to_string(),
        Some("deepseek-legacy") => {
            "With --reasoning-format deepseek-legacy the <think> tags stay inside \
             message.content and the same text is also copied to message.reasoning_content."
                .to_string()
        }
        _ => "The reasoning trace comes back as message.reasoning_content \
              (delta.reasoning_content when streaming) on /v1/chat/completions, as a thinking \
              block on /v1/messages, and as a reasoning item on /v1/responses."
            .to_string(),
    }
}

/// Notes about the projector — the three cases where what is on disk and what
/// the row configured do not line up (design §3.3).
fn projector_notes(
    projector: ProjectorStatus<'_>,
    summary: Option<&ModelSummary>,
    no_mmproj: bool,
) -> Vec<String> {
    match projector {
        ProjectorStatus::None => Vec::new(),
        ProjectorStatus::Configured { path } => {
            // Configured but silent (or unread): the modalities were withheld,
            // so say why rather than leaving a hole.
            let states_nothing = summary
                .is_none_or(|s| s.has_vision_encoder.is_none() && s.has_audio_encoder.is_none());
            if states_nothing {
                vec![format!(
                    "The configured projector {path} states neither a vision nor an audio \
                     encoder in its header, so this model's input modalities are unknown."
                )]
            } else {
                Vec::new()
            }
        }
        ProjectorStatus::Unreadable { path, err } => vec![format!(
            "The configured projector {path} could not be read ({err}), so this model's input \
             modalities are unknown."
        )],
        // `--no-mmproj` is a decision, not an oversight (it is how the
        // text-only twin of a multimodal repo is served), so it gets a
        // sentence that says so rather than one telling the owner to fix it.
        ProjectorStatus::SiblingPresentNotConfigured { path } if no_mmproj => vec![format!(
            "A projector file sits next to the weights ({path}) but --no-mmproj is set on this \
             row, so it runs text-only on purpose."
        )],
        ProjectorStatus::SiblingPresentNotConfigured { path } => vec![format!(
            "A projector file sits next to the weights ({path}) but no mmproj is configured on \
             this row, and llama-server started with -m never loads one by itself — so this \
             model is text-only until you set it."
        )],
    }
}

/// The `max_output_tokens` sentence (design §2.1): a real cap, or the reason
/// there is none.
fn max_output_note(max_output_tokens: Option<u64>) -> String {
    match max_output_tokens {
        Some(n) => format!(
            "max_output_tokens is the configured --n-predict: llama-server stops a response at \
             {n} tokens even when the request asks for more."
        ),
        None => "No output cap is configured on this model: a response may run to the end of \
                 the context window. Set n_predict on the model to publish one."
            .to_string(),
    }
}

/// The clamp sentence for a guarded row (unified-KV design §3.3 "When it is
/// active", ladder design §3.2): [`LlamaParams::pool_guarded`] rows have
/// their `max_tokens` bound to `n_predict` on every request, and the response
/// says so when it happened. `None` for every other row — an unguarded row
/// (including one that only reaches a shared pool through llama-server's own
/// auto default) is not touched by the gate at all, so nothing here would be
/// true for it.
fn pool_guard_note(p: &LlamaParams) -> Option<String> {
    p.pool_guarded().then(|| {
        "This model's KV cache is one pool shared by its slots, and lmgw guards it: a max_tokens \
         above n_predict is lowered to it before the request is sent, and the response carries \
         x-lmgw-max-tokens-clamped when that happened."
            .to_string()
    })
}

/// The "this is a ladder" sentence (ladder design §4.4): how many rungs, and
/// what a miss on the running one costs. Fires whenever the row has any
/// higher rungs configured, independent of whether the weights could be
/// read — a ladder is a fact about the row, not about this call's GGUF read.
fn ladder_note(model: &LocalModel) -> Option<String> {
    model.is_ladder().then(|| {
        format!(
            "ladder, {} rungs; a request past a rung's switchover waits for a model reload.",
            model.ladder.len() + 1
        )
    })
}

// ---------------------------------------------------------------------------
// Aux rows
// ---------------------------------------------------------------------------

/// Notes for an aux (embedding/rerank) row. `projector` is the `--mmproj`
/// value found in the row's freeform args, `weights_unreadable` says the
/// caller could not read the GGUF.
pub fn notes_for_aux(
    model: &AuxModel,
    projector: Option<&str>,
    weights_unreadable: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    match model.kind {
        AuxKind::Embed => out.push(
            "Embeddings: POST /v1/embeddings with this model id and an input that is either a \
             string or an array of strings."
                .to_string(),
        ),
        AuxKind::Rerank => out.push(
            "Reranking: POST /v1/rerank with a query and a documents array. Never send this \
             model to /v1/embeddings — llama-server answers a reranker there with an all-zero \
             vector, so lmgw refuses that combination outright."
                .to_string(),
        ),
    }
    if let Some(path) = projector {
        out.push(format!(
            "This is a multimodal embedder: it loads the projector {path}, so its input \
             modalities beyond text are not published here. Send text, and image_url parts on \
             /v1/embeddings if your client supports them."
        ));
    }
    if weights_unreadable {
        out.push(format!(
            "The model file {} could not be read; everything above comes from this row's \
             configuration alone.",
            model.gguf_path
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Audio rows
// ---------------------------------------------------------------------------

/// Notes for an audio.cpp row: the request shape of the route its task
/// actually has.
pub fn notes_for_audio(model: &AudioModel) -> Vec<String> {
    let mut out = Vec::new();
    match model.task.as_str() {
        "tts" => {
            out.push(
                "Speech: POST /v1/audio/speech with a JSON body of {model, input, voice}, \
                 where input is the text to speak and voice names one of this model's presets. \
                 GET /v1/audio/voices?model=<this id> lists the voice ids and presets it accepts."
                    .to_string(),
            );
            if model.voice_presets.is_empty() && model.default_voice_preset.is_none() {
                out.push(
                    "No voice presets are configured on this row: a model that ships its own \
                     voice ids still answers, but a cloning model draws a new random speaker for \
                     every request until presets are configured."
                        .to_string(),
                );
            } else {
                let mut names: Vec<&str> = model.voice_presets.keys().map(String::as_str).collect();
                names.sort_unstable();
                out.push(format!(
                    "Voice presets configured on this row: {}{}.",
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    },
                    if model.default_voice_preset.is_some() {
                        " (one of them is the default, used when a request names no voice)"
                    } else {
                        ""
                    }
                ));
            }
        }
        "asr" => {
            out.push(
                "Transcription: POST /v1/audio/transcriptions either as multipart/form-data with \
                 the fields file and model, or as JSON {model, audio} where audio is a path the \
                 container can read (the audio models dir is mounted at /models)."
                    .to_string(),
            );
            out.push(
                "The same request to POST /v1/audio/transcriptions/details answers with the \
                 word timings, segments and speaker turns this model produced, where it \
                 produces any — the plain route returns text and timing only. Streaming \
                 (stream=true) belongs to the plain route; the detail arrays have nowhere to \
                 go in a transcript delta."
                    .to_string(),
            );
        }
        "align" => out.push(
            "Forced alignment: POST /v1/audio/alignments as multipart/form-data with the \
             fields file, model and text (language optional) — the clip need not exist on \
             the container's filesystem. POST /v1/tasks/run reaches the same model with a \
             server-local path instead."
                .to_string(),
        ),
        task => out.push(format!(
            "Task '{task}': POST /v1/tasks/run (or /v1/tasks/stream) with {{model, request}}; \
             the nested request object is audio.cpp's own field set for this task and lmgw \
             relays it untouched."
        )),
    }
    out
}

// ---------------------------------------------------------------------------
// Image rows (stable-diffusion.cpp)
// ---------------------------------------------------------------------------

/// One `args` value as it reads in a sentence: a JSON string unquoted,
/// anything else as its JSON text. `None` is the row leaving the flag out,
/// which means sd-server's own default applies — and lmgw does **not** know
/// what that is (it is per build and per family), so it says exactly that
/// rather than printing a number it made up.
fn arg_default(model: &ImageModel, key: &str) -> String {
    match model.args.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "server default".to_string(),
    }
}

/// Notes for an image row (image-generation design §5): the two request
/// shapes, what this row's flags make the defaults, the one extension that
/// reaches everything the OpenAI body has no field for, and — only while the
/// model's container is up and has answered its capabilities route — what the
/// loaded pipeline itself accepts.
pub fn notes_for_image(model: &ImageModel, probed: Option<&ImageCapabilities>) -> Vec<String> {
    let mut out = vec![
        "Image generation: POST /v1/images/generations with {model, prompt} and optionally n, \
         size (WIDTHxHEIGHT), output_format (png|jpeg|webp) and output_compression (0-100). \
         sd-server reads no other OpenAI field — response_format, quality, style, background \
         and user are ignored, not refused."
            .to_string(),
        "Images come back as b64_json only: the response is {created, output_format, data: \
         [{b64_json}]}. There is no url mode on this server."
            .to_string(),
    ];

    if model.edit {
        out.push(
            "Editing: POST /v1/images/edits as multipart/form-data with the fields model, \
             prompt and image (plus optional mask, n, size, output_format). This row is \
             marked edit, so its pipeline takes reference images."
                .to_string(),
        );
    } else {
        out.push(
            "This row is not marked edit, so lmgw refuses POST /v1/images/edits for it before \
             the request reaches the container: a reference-image request against a pipeline \
             that cannot take one kills the server process instead of failing."
                .to_string(),
        );
    }

    out.push(format!(
        "Generation defaults this row starts sd-server with: width {}, height {}, steps {}, \
         cfg-scale {} — \"server default\" means the row sets no such flag and sd-server's own \
         default applies. A request's own size and the sd_cpp_extra_args block below override \
         them per call.",
        arg_default(model, "width"),
        arg_default(model, "height"),
        arg_default(model, "steps"),
        arg_default(model, "cfg_scale"),
    ));

    out.push(
        "Everything the OpenAI body has no field for — negative prompt, sampler, scheduler, \
         seed, LoRA, hires, tiling, cache — goes inside the prompt as one JSON block, e.g. \
         a lovely cat <sd_cpp_extra_args>{\"negative_prompt\":\"blurry\",\"seed\":42,\
         \"sample_params\":{\"sample_steps\":8,\"guidance\":{\"txt_cfg\":1.0}}}\
         </sd_cpp_extra_args>. Its schema is sd-server's own img_gen request body, and the \
         server strips the block before generating."
            .to_string(),
    );

    out.push(
        "CFG inside that block is sample_params.guidance.txt_cfg — a sample_params.cfg_scale \
         beside it parses and is then never read (measured: it returns the baseline image \
         byte for byte, while guidance.txt_cfg changes it). The --cfg-scale flag above is the \
         container's start-up default; this is how one request overrides it."
            .to_string(),
    );

    if let Some(caps) = probed {
        out.extend(probed_image_notes(caps));
    }
    out
}

/// The sentences that are only true while this row's container is up — read
/// from the `GET /sdcpp/v1/capabilities` body it answered with. Each is
/// emitted only when the server actually reported the numbers behind it.
fn probed_image_notes(caps: &ImageCapabilities) -> Vec<String> {
    let mut out = Vec::new();
    let l = &caps.limits;
    let mut bounds = Vec::new();
    if let (Some(min), Some(max)) = (l.min_width, l.max_width) {
        bounds.push(format!("widths {min}-{max}"));
    }
    if let (Some(min), Some(max)) = (l.min_height, l.max_height) {
        bounds.push(format!("heights {min}-{max}"));
    }
    if let Some(n) = l.max_batch_count {
        bounds.push(format!("n up to {n}"));
    }
    if !bounds.is_empty() {
        out.push(format!(
            "The pipeline currently loaded in this row's container accepts {} (pixels), as \
             reported by the container itself.",
            bounds.join(", ")
        ));
    }
    if !caps.samplers.is_empty() {
        out.push(format!(
            "Samplers this pipeline reports: {}. Schedulers: {}. Name one through \
             sample_params.sample_method / sample_params.scheduler inside sd_cpp_extra_args.",
            caps.samplers.join(", "),
            if caps.schedulers.is_empty() {
                "none reported".to_string()
            } else {
                caps.schedulers.join(", ")
            }
        ));
    }
    if !caps.loras.is_empty() {
        let names: Vec<&str> = caps.loras.iter().map(|a| a.name.as_str()).collect();
        out.push(format!(
            "LoRAs found in this row's LoRA directory: {}. Apply one through the lora field of \
             sd_cpp_extra_args ([{{path, multiplier}}]) — <lora:...> prompt tags are refused by \
             this server.",
            names.join(", ")
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Cloud catalog entries
// ---------------------------------------------------------------------------

/// Notes for a cloud model: what the catalog did not say, and how the
/// reasoning controls translate on this protocol (design §4, §5.3).
pub fn notes_for_catalog(
    info: &ModelInfo,
    protocol: Protocol,
    upstream_name: &str,
    caps: &ModelCapabilities,
) -> Vec<String> {
    let mut out = Vec::new();

    // A Gemini entry with no generateContent method is listed by the provider
    // but unreachable through lmgw (bidi/live-session APIs, §4).
    let bidi_only =
        protocol == Protocol::Gemini && caps.task == "chat" && info.tools == Some(false);

    if caps.input_modalities.is_none() {
        out.push(format!(
            "The catalog of upstream {upstream_name} does not state this model's input \
             modalities, so none are published here; unknown is not the same as text-only."
        ));
    }
    if bidi_only {
        // What the catalog says, not what lmgw guesses it means: a missing
        // generateContent is the fact, "it is a live-session API" was an
        // inference that happens to be wrong for some entries.
        out.push(
            "The catalog lists no generateContent method for this model, so lmgw has no route \
             to it even though it is listed here."
                .to_string(),
        );
    }

    match caps.reasoning.as_ref() {
        None => out.push(format!(
            "The catalog of upstream {upstream_name} lists no reasoning parameter for this \
             model, which is not a statement that it cannot think: no reasoning capability is \
             published and lmgw sends no reasoning control unless a request sets one."
        )),
        Some(r) if r.kind == "fixed" => out.push(
            "The provider catalog states this model does not think; the reasoning headers are \
             ignored on it."
                .to_string(),
        ),
        Some(r) => {
            let levels = if r.levels.is_empty() {
                String::new()
            } else {
                format!(" (levels: {})", r.levels.join("|"))
            };
            match protocol {
                Protocol::Openai => out.push(format!(
                    "Reasoning: set body reasoning_effort or header x-lmgw-reasoning-effort{levels}; \
                     x-lmgw-reasoning: off is sent upstream as reasoning_effort: \"none\". The \
                     catalog does not state the default state."
                )),
                Protocol::Anthropic => {
                    out.push(format!(
                        "Reasoning: header x-lmgw-reasoning-effort{levels} becomes \
                         output_config.effort with thinking type adaptive, and x-lmgw-reasoning: \
                         on|off becomes thinking type adaptive|disabled."
                    ));
                    out.push(
                        "x-lmgw-reasoning-budget is forwarded as thinking.budget_tokens, which \
                         current Claude models reject — use an effort level instead."
                            .to_string(),
                    );
                }
                Protocol::Gemini => out.push(
                    "Reasoning: x-lmgw-reasoning-budget sets \
                     generationConfig.thinkingConfig.thinkingBudget on this route and a budget of \
                     0 turns thinking off; x-lmgw-reasoning-effort sets thinkingConfig.thinkingLevel \
                     (Gemini 3 vocabulary, passed verbatim); the catalog states neither a default \
                     nor whether this model accepts 0."
                        .to_string(),
                ),
            }
        }
    }

    if info.tools == Some(false) && !bidi_only {
        out.push(
            "The provider catalog says this model takes no tools, so tool definitions sent with \
             a request will not be honoured upstream."
                .to_string(),
        );
    }

    // The Anthropic API requires `max_tokens` on every request, so lmgw always
    // has to supply one when the client does not — and which one it picks
    // depends on whether the request streams. That is surprising enough, and
    // affects long answers directly enough, that every row on this protocol
    // says it (not only the rows whose catalog publishes no maximum).
    if protocol == Protocol::Anthropic {
        out.push(if info.max_output_tokens.is_some() {
            "Non-streaming requests without max_tokens are sent with max_tokens 4096 (the \
             provider refuses larger non-streamed caps); streaming requests get the published \
             maximum; either way the response carries x-lmgw-max-tokens-defaulted. Set \
             max_tokens yourself to control it."
                .to_string()
        } else {
            "Requests without max_tokens are sent with max_tokens 4096 — this catalog publishes \
             no maximum, so streaming requests get that last-resort number too; the response \
             carries x-lmgw-max-tokens-defaulted. Set max_tokens yourself to control it."
                .to_string()
        });
    }

    out
}
