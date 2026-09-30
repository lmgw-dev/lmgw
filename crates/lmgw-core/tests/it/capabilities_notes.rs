//! `capabilities::notes` — the per-model sentences on `/v1/models`
//! (`docs/design/2026-09-17-model-capabilities-design.md` §2.1,
//! `notes` bullet).
//!
//! These assert the **whole list, verbatim**, for one representative row per
//! class and per reasoning kind / `--reasoning-format`. That is deliberate:
//! the rule the design states is "every sentence must be true for *this*
//! alias", and a `contains()` assertion cannot catch a sentence that is true
//! of some other row sneaking in. Reading a failure diff here is reading
//! exactly what a client would be told.

use lmgw_core::capabilities::{self, ProjectorStatus};
use lmgw_core::config::{
    AudioModel, AuxKind, AuxModel, ImageModel, LlamaParams, LocalModel, Protocol,
};
use lmgw_core::gguf::{ModelSummary, TemplateSignals};
use lmgw_core::runtime::image::ImageCapabilities;

const QWEN38: &str = include_str!("../fixtures/chat_templates/qwen3.8.jinja");
const GEMMA4: &str = include_str!("../fixtures/chat_templates/gemma4.jinja");
const MEDGEMMA: &str = include_str!("../fixtures/chat_templates/medgemma.jinja");
const COHERE_NORTH: &str = include_str!("../fixtures/chat_templates/cohere-north.jinja");

fn weights(tpl: &str) -> ModelSummary {
    ModelSummary {
        architecture: Some("qwen3".into()),
        has_chat_template: true,
        chat_template: Some(tpl.to_string()),
        signals: Some(TemplateSignals::from_template(tpl)),
        ..Default::default()
    }
}

fn mmproj(vision: Option<bool>, audio: Option<bool>) -> ModelSummary {
    ModelSummary {
        is_mmproj: true,
        has_vision_encoder: vision,
        has_audio_encoder: audio,
        ..Default::default()
    }
}

fn row(model_id: &str, params: LlamaParams) -> LocalModel {
    LocalModel {
        id: 1,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}/weights.gguf"),
        params,
        args: Vec::new(),
        idle_seconds: 300,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

#[track_caller]
fn assert_notes(actual: &[String], expected: &[&str]) {
    let actual: Vec<&str> = actual.iter().map(String::as_str).collect();
    assert_eq!(actual, expected);
}

// ---------------------------------------------------------------------------
// Local: levels + a vision projector + a configured cap
// ---------------------------------------------------------------------------

/// The design's own example row (§2.1): Qwen3.8 with `--reasoning on
/// --reasoning-effort low`, a vision projector and an `--n-predict`.
#[test]
fn levels_row_with_a_projector_and_a_cap() {
    let model = row(
        "qwen3.8-27b-reason-maxctx",
        LlamaParams {
            reasoning: Some("on".into()),
            reasoning_effort: Some("low".into()),
            reasoning_preserve: Some(true),
            n_predict: Some(32768),
            mmproj_path: Some("unsloth/Qwen3.8-27B-GGUF/mmproj-BF16.gguf".into()),
            ..Default::default()
        },
    );
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(QWEN38)),
        Some(&mmproj(Some(true), None)),
        None,
        ProjectorStatus::Configured {
            path: "unsloth/Qwen3.8-27B-GGUF/mmproj-BF16.gguf",
        },
        None,
    );
    assert_notes(
        &d.notes,
        &[
            "Reasoning is ON by default at effort 'low' (--reasoning on, --reasoning-effort low). Per request: header x-lmgw-reasoning-effort: low|medium|high|xhigh, or body reasoning_effort; x-lmgw-reasoning: off (or reasoning_effort: \"none\") turns it off.",
            "The reasoning trace comes back as message.reasoning_content (delta.reasoning_content when streaming) on /v1/chat/completions, as a thinking block on /v1/messages, and as a reasoning item on /v1/responses.",
            "Images: send an OpenAI image_url content part (an https URL or a data: URI) or an Anthropic image block. Projector: unsloth/Qwen3.8-27B-GGUF/mmproj-BF16.gguf.",
            "Tools: send OpenAI tools/tool_choice or Anthropic tools; llama-server parses this model's own qwen-xml tool-call syntax into structured tool_calls. Never hand-format a call in the prompt.",
            "max_output_tokens is the configured --n-predict: llama-server stops a response at 32768 tokens even when the request asks for more.",
        ],
    );
}

/// A projector with an audio encoder earns the `input_audio` sentence — and
/// only then, since claiming it on a vision-only row would be false.
#[test]
fn audio_capable_row_explains_the_input_audio_part() {
    let model = row(
        "gemma4-e4b-mm",
        LlamaParams {
            mmproj_path: Some("gemma4/mmproj.gguf".into()),
            ..Default::default()
        },
    );
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(GEMMA4)),
        Some(&mmproj(Some(true), Some(true))),
        None,
        ProjectorStatus::Configured {
            path: "gemma4/mmproj.gguf",
        },
        None,
    );
    assert_notes(
        &d.notes,
        &[
            "Reasoning is OFF by default. Per request: header x-lmgw-reasoning: on|off, or body chat_template_kwargs {\"enable_thinking\": true|false}.",
            "The reasoning trace comes back as message.reasoning_content (delta.reasoning_content when streaming) on /v1/chat/completions, as a thinking block on /v1/messages, and as a reasoning item on /v1/responses.",
            "Images: send an OpenAI image_url content part (an https URL or a data: URI) or an Anthropic image block. Projector: gemma4/mmproj.gguf.",
            "Audio: send OpenAI input_audio parts {data: base64, format: wav|mp3|flac}; llama.cpp sniffs the actual format itself, so the format field only has to be present.",
            "Tools: send OpenAI tools/tool_choice or Anthropic tools; llama-server parses this model's own gemma tool-call syntax into structured tool_calls. Never hand-format a call in the prompt.",
            "No output cap is configured on this model: a response may run to the end of the context window. Set n_predict on the model to publish one.",
        ],
    );
}

/// `--reasoning-format none` moves the trace back into `message.content`, so
/// the sentence about `reasoning_content` must not be published.
#[test]
fn toggle_row_with_reasoning_format_none() {
    let model = row(
        "gemma4-e4b",
        LlamaParams {
            reasoning: Some("off".into()),
            reasoning_format: Some("none".into()),
            ..Default::default()
        },
    );
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(GEMMA4)),
        None,
        None,
        ProjectorStatus::None,
        None,
    );
    assert_notes(
        &d.notes,
        &[
            "Reasoning is OFF by default (--reasoning off). Per request: header x-lmgw-reasoning: on|off, or body chat_template_kwargs {\"enable_thinking\": true|false}.",
            "The reasoning trace is not separated out on this row (--reasoning-format none): the thoughts stay inside message.content.",
            "Tools: send OpenAI tools/tool_choice or Anthropic tools; llama-server parses this model's own gemma tool-call syntax into structured tool_calls. Never hand-format a call in the prompt.",
            "No output cap is configured on this model: a response may run to the end of the context window. Set n_predict on the model to publish one.",
        ],
    );
}

/// `deepseek-legacy` keeps the tags in `content` *and* copies them out; a
/// template with no `enable_thinking` variable cannot be switched off; a
/// projector sitting unconfigured next to the weights is named.
#[test]
fn toggle_row_that_cannot_be_disabled_with_deepseek_legacy_format() {
    let model = row(
        "north-104b",
        LlamaParams {
            reasoning_format: Some("deepseek-legacy".into()),
            ..Default::default()
        },
    );
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(COHERE_NORTH)),
        None,
        None,
        ProjectorStatus::SiblingPresentNotConfigured {
            path: "north-104b/mmproj-F16.gguf",
        },
        None,
    );
    assert_notes(
        &d.notes,
        &[
            "Reasoning is ON by default. Per request: body reasoning_effort (or header x-lmgw-reasoning-effort) reaches this template's own reasoning_effort variable.",
            "Switching thinking off per request is not possible on this model: its chat template does not read enable_thinking, which is the only switch llama-server has to set.",
            "With --reasoning-format deepseek-legacy the <think> tags stay inside message.content and the same text is also copied to message.reasoning_content.",
            "A projector file sits next to the weights (north-104b/mmproj-F16.gguf) but no mmproj is configured on this row, and llama-server started with -m never loads one by itself — so this model is text-only until you set it.",
            "Tools: this model's chat template renders tools in a syntax lmgw does not recognise; llama-server may return calls as plain text in message.content instead of structured tool_calls, so a caller has to parse them itself.",
            "No output cap is configured on this model: a response may run to the end of the context window. Set n_predict on the model to publish one.",
        ],
    );
}

/// A template that says nothing: no trace sentence at all, and the tools
/// sentence says the definitions are dropped rather than staying silent.
#[test]
fn fixed_row_that_does_not_reason() {
    let model = row("medgemma-27b", LlamaParams::default());
    let d = capabilities::for_local_row(
        &model,
        Some(&weights(MEDGEMMA)),
        None,
        None,
        ProjectorStatus::None,
        None,
    );
    assert_notes(
        &d.notes,
        &[
            "This model does not reason: its chat template renders no thinking trace and reads no thinking variable, so the reasoning headers do nothing on it.",
            "Tools: this model's chat template has no native tool rendering; llama-server falls back to its generic prompt-injected tool format, which is unreliable — prefer a model whose tool_calls.kind is native.",
            "No output cap is configured on this model: a response may run to the end of the context window. Set n_predict on the model to publish one.",
        ],
    );
}

#[test]
fn unreadable_weights_say_which_file() {
    let model = row(
        "broken-7b",
        LlamaParams {
            n_predict: Some(1024),
            ..Default::default()
        },
    );
    let d = capabilities::for_local_row(&model, None, None, None, ProjectorStatus::None, None);
    assert_notes(
        &d.notes,
        &[
            "The model file broken-7b/weights.gguf could not be read, so nothing about this model's template, reasoning or modalities could be derived; only the row's own settings are published.",
            "max_output_tokens is the configured --n-predict: llama-server stops a response at 1024 tokens even when the request asks for more.",
        ],
    );
}

// ---------------------------------------------------------------------------
// Aux + audio rows
// ---------------------------------------------------------------------------

fn aux(model_id: &str, kind: AuxKind, args: &[&str]) -> AuxModel {
    AuxModel {
        id: 7,
        model_id: model_id.into(),
        gguf_path: format!("{model_id}/weights.gguf"),
        kind,
        pooling: None,
        ctx_size: None,
        args: args.iter().map(|s| s.to_string()).collect(),
        idle_seconds: 0,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

#[test]
fn embedder_and_reranker_notes() {
    let embed = aux("qwen3-embedding-4b", AuxKind::Embed, &[]);
    let summary = ModelSummary::default();
    assert_notes(
        &capabilities::for_aux(&embed, Some(&summary)).notes,
        &["Embeddings: POST /v1/embeddings with this model id and an input that is either a string or an array of strings."],
    );

    let rerank = aux("bge-reranker-v2", AuxKind::Rerank, &[]);
    assert_notes(
        &capabilities::for_aux(&rerank, Some(&summary)).notes,
        &["Reranking: POST /v1/rerank with a query and a documents array. Never send this model to /v1/embeddings — llama-server answers a reranker there with an all-zero vector, so lmgw refuses that combination outright."],
    );
}

fn audio(model_id: &str, task: &str) -> AudioModel {
    AudioModel {
        id: 9,
        model_id: model_id.into(),
        family: "qwen3_tts".into(),
        path: format!("audio/{model_id}"),
        task: task.into(),
        mode: "offline".into(),
        lazy: None,
        busy_timeout_ms: None,
        load_options: Default::default(),
        session_options: Default::default(),
        default_request_options: Default::default(),
        model_spec_override: None,
        config_id: None,
        weight_id: None,
        voice_presets: Default::default(),
        default_voice_preset: None,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
    }
}

#[test]
fn tts_and_asr_notes_carry_the_real_request_shapes() {
    assert_notes(
        &capabilities::for_audio(&audio("qwen3-tts", "tts")).notes,
        &[
            "Speech: POST /v1/audio/speech with a JSON body of {model, input, voice}, where input is the text to speak and voice names one of this model's presets. GET /v1/audio/voices?model=<this id> lists the voice ids and presets it accepts.",
            "No voice presets are configured on this row: a model that ships its own voice ids still answers, but a cloning model draws a new random speaker for every request until presets are configured.",
        ],
    );

    assert_notes(
        &capabilities::for_audio(&audio("parakeet", "asr")).notes,
        &[
            "Transcription: POST /v1/audio/transcriptions either as multipart/form-data with the fields file and model, or as JSON {model, audio} where audio is a path the container can read (the audio models dir is mounted at /models).",
            "The same request to POST /v1/audio/transcriptions/details answers with the word timings, segments and speaker turns this model produced, where it produces any — the plain route returns text and timing only. Streaming (stream=true) belongs to the plain route; the detail arrays have nowhere to go in a transcript delta.",
        ],
    );

    assert_notes(
        &capabilities::for_audio(&audio("qwen3-align", "align")).notes,
        &["Forced alignment: POST /v1/audio/alignments as multipart/form-data with the fields file, model and text (language optional) — the clip need not exist on the container's filesystem. POST /v1/tasks/run reaches the same model with a server-local path instead."],
    );
}

// ---------------------------------------------------------------------------
// Cloud rows
// ---------------------------------------------------------------------------

fn catalog_entry(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/tests/fixtures/catalogs/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

#[test]
fn openai_protocol_catalog_notes() {
    let info =
        lmgw_core::catalog::parse_openai_entry(&catalog_entry("kilo_anthropic_variants.json"))
            .expect("parses");
    assert_notes(
        &capabilities::for_catalog(&info, Protocol::Openai, "kilo").notes,
        &["Reasoning: set body reasoning_effort or header x-lmgw-reasoning-effort (levels: low|medium|high|xhigh|max); x-lmgw-reasoning: off is sent upstream as reasoning_effort: \"none\". The catalog does not state the default state."],
    );
}

/// Gemini publishes no modalities and lmgw expresses reasoning there as a
/// budget, so both facts are stated rather than implied.
#[test]
fn gemini_catalog_notes() {
    let info = lmgw_core::catalog::parse_gemini_entry(&catalog_entry("gemini_thinking.json"))
        .expect("parses");
    assert_notes(
        &capabilities::for_catalog(&info, Protocol::Gemini, "gemini").notes,
        &[
            "The catalog of upstream gemini does not state this model's input modalities, so none are published here; unknown is not the same as text-only.",
            "Reasoning: x-lmgw-reasoning-budget sets generationConfig.thinkingConfig.thinkingBudget on this route and a budget of 0 turns thinking off; x-lmgw-reasoning-effort sets thinkingConfig.thinkingLevel (Gemini 3 vocabulary, passed verbatim); the catalog states neither a default nor whether this model accepts 0.",
        ],
    );
}

/// The Anthropic arm: effort is the control that works, the budget is the one
/// current Claude models reject.
#[test]
fn anthropic_catalog_notes() {
    let envelope = catalog_entry("anthropic_models_list.json");
    let entry = envelope["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "claude-opus-4-8")
        .unwrap()
        .clone();
    let info = lmgw_core::catalog::parse_anthropic_entry(&entry).expect("parses");
    assert_notes(
        &capabilities::for_catalog(&info, Protocol::Anthropic, "anthropic").notes,
        &[
            "Reasoning: header x-lmgw-reasoning-effort (levels: low|medium|high|xhigh|max) becomes output_config.effort with thinking type adaptive, and x-lmgw-reasoning: on|off becomes thinking type adaptive|disabled.",
            "x-lmgw-reasoning-budget is forwarded as thinking.budget_tokens, which current Claude models reject — use an effort level instead.",
            "Non-streaming requests without max_tokens are sent with max_tokens 4096 (the provider refuses larger non-streamed caps); streaming requests get the published maximum; either way the response carries x-lmgw-max-tokens-defaulted. Set max_tokens yourself to control it.",
        ],
    );
}

// ---------------------------------------------------------------------------
// Image rows
// ---------------------------------------------------------------------------

fn image(model_id: &str) -> ImageModel {
    ImageModel {
        id: 11,
        model_id: model_id.into(),
        files: serde_json::Map::new(),
        args: serde_json::Map::new(),
        modes: vec![],
        edit: false,
        enabled: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        idle_seconds: 0,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        peak_extra_bytes: None,
        peak_learned_at: None,
    }
}

/// A row that has never run: the two request shapes, the defaults its own
/// `args` set (and "server default" for the ones it leaves alone — no family
/// number is invented), and the one extension that reaches everything else.
/// Nothing here describes a pipeline, because no pipeline has been loaded.
#[test]
fn image_row_notes_come_from_the_rows_own_flags() {
    let mut row = image("z-image-turbo");
    row.args.insert("width".into(), serde_json::json!(1024));
    row.args.insert("steps".into(), serde_json::json!(8));
    row.args.insert("cfg_scale".into(), serde_json::json!(1.0));
    assert_notes(
        &capabilities::for_image(&row, None).notes,
        &[
            "Image generation: POST /v1/images/generations with {model, prompt} and optionally n, size (WIDTHxHEIGHT), output_format (png|jpeg|webp) and output_compression (0-100). sd-server reads no other OpenAI field — response_format, quality, style, background and user are ignored, not refused.",
            "Images come back as b64_json only: the response is {created, output_format, data: [{b64_json}]}. There is no url mode on this server.",
            "This row is not marked edit, so lmgw refuses POST /v1/images/edits for it before the request reaches the container: a reference-image request against a pipeline that cannot take one kills the server process instead of failing.",
            "Generation defaults this row starts sd-server with: width 1024, height server default, steps 8, cfg-scale 1.0 — \"server default\" means the row sets no such flag and sd-server's own default applies. A request's own size and the sd_cpp_extra_args block below override them per call.",
            "Everything the OpenAI body has no field for — negative prompt, sampler, scheduler, seed, LoRA, hires, tiling, cache — goes inside the prompt as one JSON block, e.g. a lovely cat <sd_cpp_extra_args>{\"negative_prompt\":\"blurry\",\"seed\":42,\"sample_params\":{\"sample_steps\":8,\"guidance\":{\"txt_cfg\":1.0}}}</sd_cpp_extra_args>. Its schema is sd-server's own img_gen request body, and the server strips the block before generating.",
            "CFG inside that block is sample_params.guidance.txt_cfg — a sample_params.cfg_scale beside it parses and is then never read (measured: it returns the baseline image byte for byte, while guidance.txt_cfg changes it). The --cfg-scale flag above is the container's start-up default; this is how one request overrides it.",
        ],
    );
}

/// An edit row is told how to send an edit instead of why it cannot.
#[test]
fn an_edit_row_gets_the_multipart_shape() {
    let mut row = image("flux-kontext");
    row.edit = true;
    let notes = capabilities::for_image(&row, None).notes;
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("Editing: POST /v1/images/edits as multipart/form-data")),
        "{notes:#?}"
    );
    assert!(
        !notes.iter().any(|n| n.contains("not marked edit")),
        "{notes:#?}"
    );
}

/// Once the model's container has answered its capabilities route, the notes
/// carry what that *pipeline* accepts — the real numbers, from the server, and
/// only while they are real.
#[test]
fn a_probed_row_adds_what_the_container_reported() {
    let raw = std::fs::read_to_string(format!(
        "{}/tests/fixtures/sdcpp/capabilities-z-image-turbo-c678dfe.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the spike's capabilities fixture");
    let probed = ImageCapabilities::parse(&raw).expect("parses");
    let notes = capabilities::for_image(&image("z-image-turbo"), Some(&probed)).notes;

    assert_eq!(
        notes[notes.len() - 2],
        "The pipeline currently loaded in this row's container accepts widths 64-4096, heights 64-4096, n up to 8 (pixels), as reported by the container itself."
    );
    let samplers = notes.last().expect("a samplers note");
    assert!(
        samplers.starts_with("Samplers this pipeline reports: euler, euler_a,"),
        "{samplers}"
    );
    assert!(
        samplers.contains("Schedulers: discrete, normal, karras,"),
        "{samplers}"
    );
    // This pipeline found no LoRAs, so nothing claims it has any.
    assert!(
        !notes.iter().any(|n| n.contains("LoRAs found")),
        "{notes:#?}"
    );
}
