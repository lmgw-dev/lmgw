//! `capabilities::for_catalog` / `for_aux` / `for_audio` / `for_image` — the classes whose
//! facts come from a provider catalog or from the owner's own configuration,
//! per `docs/design/2026-09-17-model-capabilities-design.md` §4 and
//! §2.1.
//!
//! The cloud cases run the real captured catalog entries under
//! `tests/fixtures/catalogs/` through `catalog::parse_*_entry` first, so this
//! covers the whole path from provider JSON to the published object rather
//! than a hand-made `ModelInfo` that could drift from what the parsers emit.

use lmgw_core::capabilities::{self, ModelCapabilities, StructuredOutputCaps};
use lmgw_core::catalog::{
    parse_anthropic_entry, parse_gemini_entry, parse_openai_entry, ModelInfo,
};
use lmgw_core::config::{AudioModel, AuxKind, AuxModel, ImageModel, Protocol};
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/catalogs/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn anthropic_entry(id: &str) -> Value {
    let envelope = fixture("anthropic_models_list.json");
    envelope["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} not in fixture"))
        .clone()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

const CHAT_OPENAI: [&str; 4] = [
    "/v1/chat/completions",
    "/v1/messages",
    "/v1/responses",
    "/v1/completions",
];
const CHAT_OTHER: [&str; 3] = ["/v1/chat/completions", "/v1/messages", "/v1/responses"];

// ---------------------------------------------------------------------------
// OpenAI-protocol catalogs (Kilo/OpenRouter shape)
// ---------------------------------------------------------------------------

/// A Kilo entry with a full `opencode.variants` ladder: `levels` with
/// `can_disable`, the three OpenAI-protocol controls, both structured-output
/// flags, and the catalog's own `created` / `max_completion_tokens`.
#[test]
fn kilo_variants_model() {
    let info = parse_openai_entry(&fixture("kilo_anthropic_variants.json")).expect("parses");
    let d = capabilities::for_catalog(&info, Protocol::Openai, "kilo");
    let caps = d.capabilities.expect("capabilities");

    assert_eq!(caps.task, "chat");
    assert_eq!(caps.endpoints, strings(&CHAT_OPENAI));
    assert_eq!(caps.source, "catalog");
    assert_eq!(
        caps.input_modalities,
        Some(strings(&["text", "image", "file", "pdf"]))
    );
    assert_eq!(caps.output_modalities, Some(strings(&["text"])));
    assert_eq!(caps.vision, Some(true));

    let r = caps.reasoning.expect("reasoning");
    assert_eq!(r.kind, "levels");
    assert_eq!(r.levels, ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(r.can_disable, Some(true));
    assert_eq!(r.enabled, None, "no catalog states a default state");
    assert_eq!(r.default, None);
    assert_eq!(r.preserve_history, None);
    assert_eq!(r.budget_tokens, None);
    assert_eq!(
        r.control,
        strings(&[
            "x-lmgw-reasoning",
            "x-lmgw-reasoning-effort",
            "reasoning_effort"
        ])
    );

    let t = caps.tool_calls.expect("tool_calls");
    assert_eq!(t.kind, "native");
    assert_eq!(t.format.as_deref(), Some("provider"));
    assert_eq!(t.parallel, None, "no catalog publishes this");

    assert_eq!(
        caps.structured_output,
        Some(StructuredOutputCaps {
            json_schema: Some(true),
            json_object: Some(true)
        })
    );
    assert_eq!(d.max_output_tokens, Some(128_000));
    assert_eq!(d.created, Some(1_784_912_544));
}

/// A Kilo entry whose `supported_parameters` name no reasoning control:
/// `reasoning` is absent (not `fixed`), and the notes say why.
#[test]
fn kilo_model_without_reasoning() {
    let info = parse_openai_entry(&fixture("kilo_no_reasoning.json")).expect("parses");
    let d = capabilities::for_catalog(&info, Protocol::Openai, "kilo");
    let caps = d.capabilities.expect("capabilities");

    assert_eq!(caps.reasoning, None);
    assert_eq!(caps.vision, Some(true));
    assert_eq!(caps.tool_calls.unwrap().kind, "native");
    assert_eq!(
        caps.structured_output,
        Some(StructuredOutputCaps {
            json_schema: None,
            json_object: Some(true)
        })
    );
    assert_eq!(d.max_output_tokens, Some(131_072));
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("lists no reasoning parameter")),
        "{:?}",
        d.notes
    );
}

/// An OpenAI-protocol entry that publishes an embedding output is routable on
/// `/v1/embeddings` and nowhere else — `task` drives `endpoints` (§2.1).
#[test]
fn kilo_embedding_model_is_routed_to_embeddings_only() {
    let info = parse_openai_entry(&fixture("kilo_embedding.json")).expect("parses");
    let caps = capabilities::for_catalog(&info, Protocol::Openai, "kilo")
        .capabilities
        .expect("capabilities");
    assert_eq!(caps.task, "embedding");
    assert_eq!(caps.endpoints, ["/v1/embeddings"]);
}

/// A stock `api.openai.com`-shaped entry publishes none of the Kilo fields.
/// The route still gets a `capabilities` object — "this id exists and here is
/// where to send it" — with every unknown absent.
#[test]
fn a_catalog_that_says_nothing_still_publishes_the_route() {
    let info = parse_openai_entry(&json!({"id": "gpt-6", "object": "model"})).expect("parses");
    let d = capabilities::for_catalog(&info, Protocol::Openai, "openai");
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(
        caps,
        ModelCapabilities {
            task: "chat".to_string(),
            endpoints: strings(&CHAT_OPENAI),
            source: "catalog".to_string(),
            ..Default::default()
        }
    );
    assert_eq!(d.max_output_tokens, None);
    assert_eq!(d.created, None);
}

// ---------------------------------------------------------------------------
// Gemini
// ---------------------------------------------------------------------------

#[test]
fn gemini_thinking_model() {
    let info = parse_gemini_entry(&fixture("gemini_thinking.json")).expect("parses");
    let d = capabilities::for_catalog(&info, Protocol::Gemini, "gemini");
    let caps = d.capabilities.expect("capabilities");

    assert_eq!(caps.endpoints, strings(&CHAT_OTHER), "no /v1/completions");
    assert_eq!(caps.input_modalities, None, "the catalog states none");
    assert_eq!(caps.vision, None, "absent, not false");

    let r = caps.reasoning.expect("reasoning");
    assert_eq!(r.kind, "toggle");
    assert_eq!(r.enabled, None);
    assert_eq!(r.can_disable, None);
    assert_eq!(
        r.control,
        strings(&[
            "x-lmgw-reasoning",
            "x-lmgw-reasoning-effort",
            "x-lmgw-reasoning-budget"
        ])
    );
    assert_eq!(caps.tool_calls.unwrap().kind, "native");
    assert_eq!(caps.structured_output, None);
    assert_eq!(d.max_output_tokens, Some(65_536));
    assert_eq!(d.created, None, "Gemini publishes no timestamp");
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("does not state this model's input modalities")),
        "{:?}",
        d.notes
    );
}

#[test]
fn gemini_embedding_model() {
    let info = parse_gemini_entry(&fixture("gemini_embedding.json")).expect("parses");
    let caps = capabilities::for_catalog(&info, Protocol::Gemini, "gemini")
        .capabilities
        .expect("capabilities");
    assert_eq!(caps.task, "embedding");
    assert_eq!(caps.endpoints, ["/v1/embeddings"]);
    assert_eq!(caps.tool_calls.unwrap().kind, "none");
    assert_eq!(caps.reasoning.unwrap().kind, "fixed");
}

/// A `bidiGenerateContent`-only entry is listed by the provider but has no
/// lmgw route; the note says so instead of the object pretending otherwise.
#[test]
fn gemini_bidi_only_model_is_noted_as_unroutable() {
    let info = parse_gemini_entry(&fixture("gemini_bidi_only.json")).expect("parses");
    let d = capabilities::for_catalog(&info, Protocol::Gemini, "gemini");
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("no generateContent method")),
        "{:?}",
        d.notes
    );
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

#[test]
fn anthropic_model_with_the_full_capabilities_tree() {
    let info = parse_anthropic_entry(&anthropic_entry("claude-opus-4-8")).expect("parses");
    let d = capabilities::for_catalog(&info, Protocol::Anthropic, "anthropic");
    let caps = d.capabilities.expect("capabilities");

    assert_eq!(caps.endpoints, strings(&CHAT_OTHER));
    assert_eq!(caps.input_modalities, Some(strings(&["text", "image"])));
    assert_eq!(caps.vision, Some(true));

    let r = caps.reasoning.expect("reasoning");
    assert_eq!(r.kind, "levels");
    assert_eq!(r.levels, ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(r.enabled, None);
    assert_eq!(r.can_disable, None, "the tree has no types.disabled");
    assert_eq!(
        r.control,
        strings(&[
            "x-lmgw-reasoning",
            "x-lmgw-reasoning-effort",
            "x-lmgw-reasoning-budget",
            "thinking",
            "output_config.effort"
        ])
    );
    assert_eq!(caps.tool_calls.unwrap().kind, "native");
    assert_eq!(d.max_output_tokens, Some(128_000));
    assert_eq!(d.created, Some(1_769_904_000), "2026-02-01T00:00:00Z");
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("current Claude models reject")),
        "{:?}",
        d.notes
    );
}

/// An Anthropic entry with no `capabilities` object and no `max_tokens`: the
/// reasoning object is absent, and the note warns that lmgw will have to
/// invent a `max_tokens` (§5.4).
#[test]
fn anthropic_model_without_a_capabilities_object() {
    let info = parse_anthropic_entry(&anthropic_entry("claude-3-haiku-20240307")).expect("parses");
    let d = capabilities::for_catalog(&info, Protocol::Anthropic, "anthropic");
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(caps.reasoning, None);
    assert_eq!(caps.input_modalities, None);
    assert_eq!(d.max_output_tokens, None);
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("x-lmgw-max-tokens-defaulted")),
        "{:?}",
        d.notes
    );
}

// ---------------------------------------------------------------------------
// Aux rows
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
fn aux_embedder() {
    let model = aux("qwen3-embedding-4b", AuxKind::Embed, &[]);
    let d = capabilities::for_aux(&model, None);
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(caps.task, "embedding");
    assert_eq!(caps.endpoints, ["/v1/embeddings"]);
    assert_eq!(caps.input_modalities, Some(strings(&["text"])));
    assert_eq!(caps.output_modalities, Some(strings(&["embedding"])));
    assert_eq!(caps.vision, Some(false));
    assert_eq!(caps.reasoning, None);
    assert_eq!(caps.tool_calls.unwrap().kind, "none");
    assert_eq!(caps.structured_output, None);
    assert_eq!(caps.source, "config");
    assert_eq!(d.max_output_tokens, None);
    assert!(
        d.notes.iter().any(|n| n.contains("could not be read")),
        "an unread GGUF is still worth a note: {:?}",
        d.notes
    );
}

#[test]
fn aux_reranker() {
    let model = aux("bge-reranker", AuxKind::Rerank, &[]);
    let caps = capabilities::for_aux(&model, None)
        .capabilities
        .expect("capabilities");
    assert_eq!(caps.task, "rerank");
    assert_eq!(caps.endpoints, ["/v1/rerank"]);
    assert_eq!(
        caps.output_modalities, None,
        "relevance scores are none of the modality vocabulary"
    );
}

/// A multimodal embedder's extra input modalities live in a projector this
/// builder does not read, so the list is withheld rather than published as
/// text-only.
#[test]
fn multimodal_embedder_withholds_its_modalities() {
    let model = aux(
        "jina-clip",
        AuxKind::Embed,
        &["--mmproj", "jina/mmproj.gguf"],
    );
    let d = capabilities::for_aux(&model, None);
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(caps.input_modalities, None);
    assert_eq!(caps.vision, None);
    assert!(d
        .notes
        .iter()
        .any(|n| n.contains("jina/mmproj.gguf") && n.contains("image_url")));
}

// ---------------------------------------------------------------------------
// Audio rows
// ---------------------------------------------------------------------------

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
fn audio_tts() {
    let d = capabilities::for_audio(&audio("qwen3-tts", "tts"));
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(caps.task, "tts");
    assert_eq!(caps.endpoints, ["/v1/audio/speech", "/v1/audio/voices"]);
    assert_eq!(caps.input_modalities, Some(strings(&["text"])));
    assert_eq!(caps.output_modalities, Some(strings(&["audio"])));
    assert_eq!(caps.vision, Some(false));
    assert_eq!(caps.tool_calls.unwrap().kind, "none");
    assert_eq!(caps.source, "config");
    assert!(d
        .notes
        .iter()
        .any(|n| n.contains("/v1/audio/speech") && n.contains("{model, input, voice}")));
}

#[test]
fn audio_asr() {
    let d = capabilities::for_audio(&audio("whisper-large", "asr"));
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(caps.task, "asr");
    assert_eq!(
        caps.endpoints,
        [
            "/v1/audio/transcriptions",
            "/v1/audio/transcriptions/details"
        ]
    );
    assert_eq!(caps.input_modalities, Some(strings(&["audio"])));
    assert_eq!(caps.output_modalities, Some(strings(&["text"])));
    assert!(d
        .notes
        .iter()
        .any(|n| n.contains("multipart/form-data") && n.contains("{model, audio}")));
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("/v1/audio/transcriptions/details")),
        "an ASR row says where its word timings and speaker turns come out"
    );
}

/// Forced alignment takes both halves — a clip and the transcript to align it
/// against — and has an upload route of its own since audio.cpp grew one.
#[test]
fn audio_alignment() {
    let d = capabilities::for_audio(&audio("qwen3-align", "align"));
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(caps.task, "align");
    assert_eq!(
        caps.endpoints,
        ["/v1/audio/alignments", "/v1/tasks/run", "/v1/tasks/stream"]
    );
    assert_eq!(caps.input_modalities, Some(strings(&["audio", "text"])));
    assert_eq!(caps.output_modalities, Some(strings(&["text"])));
    assert!(d
        .notes
        .iter()
        .any(|n| n.contains("/v1/audio/alignments") && n.contains("text")));
}

/// The tasks with no OpenAI shape are reached through the generic task
/// route, and their modalities are audio.cpp's business — absent, not guessed.
#[test]
fn audio_generic_task() {
    let d = capabilities::for_audio(&audio("demucs", "sep"));
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(caps.task, "sep");
    assert_eq!(caps.endpoints, ["/v1/tasks/run", "/v1/tasks/stream"]);
    assert_eq!(caps.input_modalities, None);
    assert_eq!(caps.output_modalities, None);
    assert_eq!(caps.vision, None);
    assert!(d.notes.iter().any(|n| n.contains("Task 'sep'")));
}

// ---------------------------------------------------------------------------
// Serialization (§2.1: absent means unknown)
// ---------------------------------------------------------------------------

/// A minimal object must serialize to exactly its three known keys: no null
/// values, no empty `levels` / `control` arrays, nothing a consumer could
/// read as "the model cannot".
#[test]
fn unknown_fields_are_omitted_entirely() {
    let info = ModelInfo {
        id: "x".into(),
        context_length: None,
        pricing: None,
        created: None,
        max_output_tokens: None,
        input_modalities: None,
        output_modalities: None,
        task: None,
        reasoning: None,
        tools: None,
        structured_output: None,
    };
    let caps = capabilities::for_catalog(&info, Protocol::Openai, "u")
        .capabilities
        .unwrap();
    let v = serde_json::to_value(&caps).expect("serializes");
    // serde_json orders an object's keys alphabetically; what matters is the
    // set, i.e. that nothing unknown was emitted at all.
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["endpoints", "source", "task"]);
    assert!(!v.to_string().contains("null"));
}

/// The same rule one level down: a `fixed` reasoning object carries neither an
/// empty `levels` nor an empty `control`.
#[test]
fn empty_lists_inside_reasoning_are_omitted_too() {
    let info = parse_gemini_entry(&fixture("gemini_tts_no_thinking.json")).expect("parses");
    let caps = capabilities::for_catalog(&info, Protocol::Gemini, "gemini")
        .capabilities
        .unwrap();
    let v = serde_json::to_value(&caps).expect("serializes");
    let reasoning = v["reasoning"].as_object().expect("reasoning object");
    assert_eq!(
        reasoning.keys().map(String::as_str).collect::<Vec<_>>(),
        ["enabled", "kind"]
    );
    assert!(!v.to_string().contains("null"));
}

// ---------------------------------------------------------------------------
// Image rows (stable-diffusion.cpp) — image-generation design §5
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

/// The plain case: text in, one image out, one route, and none of the four
/// fields a diffusion pipeline has no answer for.
#[test]
fn image_generation_row() {
    let d = capabilities::for_image(&image("z-image-turbo"), None);
    let caps = d.capabilities.expect("capabilities");
    assert_eq!(caps.task, "image_generation");
    assert_eq!(caps.endpoints, ["/v1/images/generations"]);
    assert_eq!(caps.input_modalities, Some(strings(&["text"])));
    assert_eq!(caps.output_modalities, Some(strings(&["image"])));
    assert_eq!(caps.vision, Some(false));
    assert_eq!(caps.source, "config");
    // Absent stays absent: nothing here is published as a negative.
    assert_eq!(caps.reasoning, None);
    assert_eq!(caps.tool_calls, None);
    assert_eq!(caps.structured_output, None);
    assert_eq!(d.max_output_tokens, None);
    // And the serialized object carries no context window / token cap either.
    let v = serde_json::to_value(&caps).expect("serializes");
    for absent in [
        "reasoning",
        "tool_calls",
        "structured_output",
        "context_length",
        "max_output_tokens",
    ] {
        assert!(v.get(absent).is_none(), "{absent} must stay absent: {v}");
    }
}

/// An edit row gains the second route and the image input modality — and with
/// it `vision`, which is the boolean a client actually reads.
#[test]
fn image_edit_row_advertises_the_edits_route() {
    let mut row = image("flux-kontext");
    row.edit = true;
    let caps = capabilities::for_image(&row, None)
        .capabilities
        .expect("capabilities");
    assert_eq!(caps.task, "image_edit");
    assert_eq!(
        caps.endpoints,
        ["/v1/images/generations", "/v1/images/edits"]
    );
    assert_eq!(caps.input_modalities, Some(strings(&["text", "image"])));
    assert_eq!(caps.output_modalities, Some(strings(&["image"])));
    assert_eq!(caps.vision, Some(true));
}

/// A row that only does video says so, and its output modality is `video` —
/// the one place the class publishes something other than an image.
#[test]
fn video_only_row_is_published_as_video_generation() {
    let mut row = image("wan-2.2");
    row.modes = vec!["vid_gen".into()];
    let caps = capabilities::for_image(&row, None)
        .capabilities
        .expect("capabilities");
    assert_eq!(caps.task, "video_generation");
    assert_eq!(caps.output_modalities, Some(strings(&["video"])));

    // A row that does both is an image generator that can also do video: the
    // published task is what the routes can serve.
    let mut both = image("wan-2.2");
    both.modes = vec!["img_gen".into(), "vid_gen".into()];
    let caps = capabilities::for_image(&both, None)
        .capabilities
        .expect("capabilities");
    assert_eq!(caps.task, "image_generation");
    assert_eq!(caps.output_modalities, Some(strings(&["image"])));
}

/// The owner's override deep-merges over the derived object exactly as it does
/// for every other class, and the result is attributed to them.
#[test]
fn an_image_rows_override_merges_over_the_derived_object() {
    let row = image("z-image-turbo");
    let derived = capabilities::for_image(&row, None);
    let applied = capabilities::apply_owner_override(
        derived,
        &json!({
            "capabilities": {"output_modalities": ["image", "video"]},
            "notes": ["This box renders 512x512 in half a second."],
        }),
    )
    .expect("a well-formed override applies");
    let caps = applied.capabilities.expect("capabilities");
    assert_eq!(caps.output_modalities, Some(strings(&["image", "video"])));
    // Untouched keys survive the merge, and the source names the owner.
    assert_eq!(caps.task, "image_generation");
    assert_eq!(caps.endpoints, ["/v1/images/generations"]);
    assert_eq!(caps.source, "owner");
    assert!(applied
        .notes
        .iter()
        .any(|n| n.contains("renders 512x512 in half a second")));
}

/// A cloud entry whose catalog says its output is an image gets the image task
/// and the generations route — and the edits route only when the catalog also
/// says it takes an image in.
#[test]
fn a_cloud_catalog_image_model_gets_the_image_routes() {
    let entry = json!({
        "id": "black-forest-labs/flux-1.1-pro",
        "architecture": {"input_modalities": ["text"], "output_modalities": ["image"]},
    });
    let info = parse_openai_entry(&entry).expect("parses");
    assert_eq!(info.task.as_deref(), Some("image_generation"));
    let caps = capabilities::for_catalog(&info, Protocol::Openai, "kilo")
        .capabilities
        .expect("capabilities");
    assert_eq!(caps.task, "image_generation");
    assert_eq!(caps.endpoints, ["/v1/images/generations"]);
    assert_eq!(caps.output_modalities, Some(strings(&["image"])));
    assert_eq!(caps.source, "catalog");

    let edit = json!({
        "id": "black-forest-labs/flux-kontext-pro",
        "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["image"]},
    });
    let info = parse_openai_entry(&edit).expect("parses");
    let caps = capabilities::for_catalog(&info, Protocol::Openai, "kilo")
        .capabilities
        .expect("capabilities");
    assert_eq!(
        caps.endpoints,
        ["/v1/images/generations", "/v1/images/edits"]
    );
    assert_eq!(caps.vision, Some(true));
}

/// A chat model that can *also* draw (`[text, image]` out, which is how the
/// Gemini image models appear in an OpenRouter-shaped catalog) keeps its chat
/// routes. `task` picks the endpoints, so calling it an image generator would
/// take `/v1/chat/completions` — the route it is actually used through — away
/// from it.
#[test]
fn a_chat_model_that_also_outputs_images_stays_chat() {
    let entry = json!({
        "id": "google/gemini-3-flash-image",
        "architecture": {
            "input_modalities": ["text", "image"],
            "output_modalities": ["text", "image"],
        },
    });
    let info = parse_openai_entry(&entry).expect("parses");
    assert_eq!(info.task.as_deref(), Some("chat"));
    let caps = capabilities::for_catalog(&info, Protocol::Openai, "kilo")
        .capabilities
        .expect("capabilities");
    assert_eq!(caps.task, "chat");
    assert!(caps.endpoints.iter().any(|e| e == "/v1/chat/completions"));
}
