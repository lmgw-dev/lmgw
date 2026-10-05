//! What audio.cpp's specs do not say about a family, written down once.
//!
//! Everything else in the speech profile ([`super::profile`]) is read from
//! the spec (the catalog snapshot, or the one a package GGUF embeds) and the
//! package's own files. This table holds the rest, each row read off
//! audio.cpp's source: which families keep their speaker and language
//! tables in an embedded `config.json`, what an engine does with a
//! language ([`language`]: every text-to-speech family, and the
//! speech-to-text ones whose source says more than their spec), which
//! inline tags a tokenizer
//! renders, which engines read instructions their spec does not declare, and
//! what a Qwen3-TTS variant does with them, which take a `seed` their spec
//! does not declare, what an engine does when no voice is named, which
//! engines refuse to clone a clip without its transcript, and which loaders
//! choose their weights by the row's `weight`.

use super::profile::{InstructionsMode, Unvoiced};

/// What each family's engine does with a language, read off its source.
mod language;
pub(crate) use language::{
    hears_language, language_regions, package_language, reads_prompt_dictionary, takes_language,
    voice_language, HearsLanguage, PackageLanguage, TakesLanguage,
};

/// Families whose speakers (`talker_config.spk_id`), languages
/// (`talker_config.codec_language_id`) and variant (`tts_model_type`) live
/// in the package's embedded `config.json` — Qwen3-TTS
/// (`src/models/qwen3_tts/assets.cpp`). The spec lists none of them.
pub(crate) fn reads_qwen3_config(family: &str) -> bool {
    family == "qwen3_tts"
}

/// Families whose loader picks its weights among named assets by the row's
/// `weight` (`select_named_asset(weights, request.weight_id, "weight")` in
/// `silero_vad/session.cpp` and `marblenet_vad/session.cpp`, the only
/// readers of `weight_id` in audio.cpp's source). No spec declares this as
/// an option. Such a row keeps its directory and its `weight`: handing it
/// one file (`files::direct_gguf`) would take the choice away from it.
pub(crate) fn reads_weight(family: &str) -> bool {
    matches!(family, "silero_vad" | "marblenet_vad")
}

/// The `--task` a Qwen3-TTS variant (`tts_model_type`) runs: CustomVoice
/// and Base speak `tts` (Base clones from a reference), VoiceDesign only
/// `vdes` (`qwen3_tts/session.cpp`, which refuses the other task).
pub(crate) fn qwen3_variant_task(tts_model_type: &str) -> Option<&'static str> {
    match tts_model_type {
        "custom_voice" | "base" => Some("tts"),
        "voice_design" => Some("vdes"),
        _ => None,
    }
}

/// The inline tags a family's tokenizer renders, in its own spelling
/// (audio-class gap 9b, [`super::tags`]). Read off audio.cpp's source:
/// OmniVoice's `nonverbal_pattern` (`omnivoice/tokenizer_text.cpp`) and
/// CosyVoice3's added `[…]` tokens (`cosyvoice3/tokenizer_text.cpp`). A
/// family not listed renders a tag as text, so its tags are stripped —
/// Fish S2 too until a live probe shows its free-form tags are heard, and
/// Chatterbox Turbo until its tokenizer's tags are read.
pub(crate) fn tag_vocabulary(family: &str) -> Option<&'static [&'static str]> {
    match family {
        "omnivoice" => Some(&[
            "laughter",
            "sigh",
            "confirmation-en",
            "question-en",
            "question-ah",
            "question-oh",
            "question-ei",
            "question-yi",
            "surprise-ah",
            "surprise-oh",
            "surprise-wa",
            "surprise-yo",
            "dissatisfaction-hnn",
        ]),
        "cosyvoice3" => Some(&[
            "breath",
            "noise",
            "laughter",
            "cough",
            "clucking",
            "accent",
            "quick_breath",
            "hissing",
            "sigh",
            "vocalized-noise",
            "lipsmack",
            "mn",
        ]),
        _ => None,
    }
}

/// Families whose tokenizer renders any bracketed tag. None is confirmed
/// yet: Fish Audio S2 documents only its `<|speaker:N|>` markup, and
/// whether `[laughs]` is heard rather than read is what the owner's live
/// probe (`tests/it/audio_tags_live.rs`) decides.
pub(crate) fn free_form_tags(_family: &str) -> bool {
    false
}

/// Families whose engine reads `options.instruction` — which audio.cpp makes
/// of a request's `instructions` (`build_speech_request`) — although their
/// spec declares no such option, and what it does with them. Read off
/// audio.cpp's source: OmniVoice reads it beside the text and speaks without
/// it too (`omnivoice/session.cpp`), so it is passed on; MOSS-VoiceGen
/// designs its voice from it (`moss_voicegen/session.cpp`; its spec lists
/// only the `design` task and no options), so a request without one is
/// refused. Without this table both would have their instructions dropped
/// on the way. It wins over the spec: it is what the engine does.
pub(crate) fn undeclared_instructions(family: &str) -> Option<InstructionsMode> {
    match family {
        "omnivoice" => Some(InstructionsMode::Passthrough),
        "moss_voicegen" => Some(InstructionsMode::VoiceDesign),
        _ => None,
    }
}

/// What a Qwen3-TTS variant does with `instructions` (`qwen3_tts/session.cpp`
/// reads `instruction` or `instruct` for both): CustomVoice takes them as a
/// speaking style for its speaker, VoiceDesign describes the voice with
/// them and cannot speak without, Base clones and reads none.
pub(crate) fn qwen3_variant_instructions(tts_model_type: &str) -> Option<InstructionsMode> {
    match tts_model_type {
        "custom_voice" => Some(InstructionsMode::Style),
        "voice_design" => Some(InstructionsMode::VoiceDesign),
        "base" => Some(InstructionsMode::None),
        _ => None,
    }
}

/// Families whose engine takes a request's `seed` although their spec
/// declares no such option. audio.cpp maps a body `seed` to `options.seed`
/// for every family (`app/server/runtime.cpp` `build_speech_request`):
/// - Qwen3-TTS reads it, and without one draws a random seed per request
///   (`qwen3_tts/session.cpp`) — which is why a voice designed from the same
///   description sounds different from one request to the next;
/// - OmniVoice (whose spec declares no options at all) parses it
///   (`omnivoice/session.cpp` `generation_options_from_options`) and seeds
///   its generator's sampler with it at the start of every request
///   (`run()` and `initialize_streaming_request`, `seed_rng`); without one
///   that sampler keeps the `std::random_device` seed it was built with
///   (`omnivoice/generator.cpp`), the only randomness in the engine — and
///   what it draws its speaker from when it is given no reference
///   ([`unvoiced`]).
pub(crate) fn reads_seed(family: &str) -> bool {
    matches!(family, "qwen3_tts" | "omnivoice")
}

/// Families whose engine **refuses** to clone reference audio — a
/// voice-library clip, a preset's `voice_ref` — without its transcript
/// (`options.reference_text`), with a 500. audio.cpp fills that option from
/// the library's `prompt_text` index (`build_speech_request`,
/// `load_voice_library_text`), so a clip with no line there fails every
/// request. The spec's `reference_text` option is no guide: OmniVoice and
/// Qwen3-TTS declare no options at all yet refuse, and CosyVoice3 declares it
/// yet speaks without (`encode_zero_shot` takes an empty one). Read off
/// audio.cpp's source (main, 2026-10-04; the same throws at 94bd465):
/// - **OmniVoice** "native voice clone currently requires reference_text
///   when reference audio is provided" (`omnivoice/prompt_builder.cpp`);
/// - **Qwen3-TTS Base**, by its variant: "voice clone ICL mode requires
///   reference text" (`qwen3_tts/prompt_tts_voice_clone.cpp`) — ICL is its
///   default, `x_vector_only_mode`, which lmgw never sends, the other mode;
/// - **Fish Audio S2** and **Audio8 TTS** "… with inline reference audio
///   requires reference_text option" (`fish_audio/session.cpp`,
///   `audio8_tts/session.cpp`);
/// - **F5-TTS**, **ZipVoice**, **GLM-TTS**, **Kitten TTS 2**, **BreezeTTS**
///   and **OuteTTS** ("… requires reference_text", "… requires
///   --reference-text", in their `session.cpp`, `prompt.cpp` or
///   `generator.cpp`).
///
/// Not listed: VoxCPM2 clones its `voice` reference without text — only
/// prompt audio (`audio_input`, continuation) wants `prompt_text`
/// (`voxcpm2/audiovae.cpp` `encode_prompt_audio`) — and CosyVoice3, above.
/// A family upstream adds later shows as audio.cpp's own 500, which lmgw
/// words as `voice_needs_transcript` all the same
/// ([`super::engine_errors`]).
pub(crate) fn clone_requires_transcript(family: &str, variant: Option<&str>) -> bool {
    match family {
        "omnivoice" | "fish_audio" | "audio8_tts" | "f5_tts" | "zipvoice" | "glm_tts"
        | "kitten_tts2" | "breeze_tts" | "outetts" => true,
        "qwen3_tts" => variant == Some("base"),
        _ => false,
    }
}

/// What a family's engine does with a speech request that names no voice
/// at all (live run 3): audio.cpp's `build_speech_request`
/// (`app/server/runtime.cpp`) hands the engine no speaker when the request
/// has no `voice`, no `voice_ref` and the row no default preset. Only what
/// the engine's source shows is listed; every other family is `None` —
/// unknown, so realtime keeps naming a voice for it (Pocket TTS refuses
/// without one, `pocket_tts/session.cpp` `prepare()`; Qwen3-TTS
/// CustomVoice wants a speaker, `prompt_tts_custom_voice.cpp`).
///
/// It speaks with a voice of its own, drawn per request:
/// - **OmniVoice** takes reference audio only when one is given
///   (`omnivoice/session.cpp` `prepare()` and `run()`), otherwise samples a
///   speaker and carries the first chunk's voice through the rest of that
///   request — a different voice per request: [`Unvoiced::DrawsSpeaker`].
///   The request's `seed` fixes the sampler ([`reads_seed`]), yet the
///   clause text still picks the speaker (live run 3c), so a realtime
///   session, a request per clause, needs a voice configured for it (R5
///   F1) and sends every clause its one seed besides (R4 M1).
///
/// It speaks with a fixed voice of its own:
/// - **MagpieTTS** falls back to its first baked speaker when no
///   `voice_id` is set (`magpie_tts/request.cpp` `parse_voice_id`, the
///   default `speaker = 0` of `types.h`);
/// - **Supertonic** speaks its `M1` style without a voice id
///   (`supertonic/session.cpp` `generation_options_from_request`, the
///   default of `session.h`);
/// - **Kokoro** speaks `af_heart` without a voice id
///   (`kokoro_tts/frontend.cpp` `resolve_voice_id`) — and refuses it when
///   the package lacks it.
///
/// It clones from reference audio and refuses without one:
/// - **CosyVoice3** ("CosyVoice3 requires reference audio",
///   `cosyvoice3/session.cpp` `run()`);
/// - **Chatterbox** ("requires speaker reference audio",
///   `chatterbox/session.cpp` `prepare()`);
/// - **IndexTTS2** (`index_tts2/request.cpp`);
/// - **MioTTS** (`miotts/session.cpp` `run()`);
/// - **Qwen3-TTS Base** ("requires voice clone reference audio",
///   `qwen3_tts/session.cpp`), by its GGUF's variant.
pub(crate) fn unvoiced(family: &str, variant: Option<&str>) -> Option<Unvoiced> {
    match family {
        "omnivoice" => Some(Unvoiced::DrawsSpeaker),
        "magpie_tts" => Some(Unvoiced::EngineDefault { voice: None }),
        "supertonic" => Some(Unvoiced::EngineDefault { voice: Some("M1") }),
        "kokoro_tts" => Some(Unvoiced::EngineDefault {
            voice: Some("af_heart"),
        }),
        "cosyvoice3" | "chatterbox" | "index_tts2" | "miotts" => Some(Unvoiced::NeedsReference),
        "qwen3_tts" if variant == Some("base") => Some(Unvoiced::NeedsReference),
        _ => None,
    }
}
