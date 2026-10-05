//! What each family's engine does with a language, read off audio.cpp's
//! source (main at 94bd465, 2026-10-04).
//!
//! audio.cpp hands a speech body's `language` to every engine as
//! `text_input.language` and never as `options.language`
//! (`app/server/runtime.cpp` `build_speech_request`); `options` arrive only
//! as the body's own `options` object. A transcription's `language` goes in
//! as `text_input.language`, and as `options.language` too where the spec
//! declares that option (`build_openai_transcription_request`). What the
//! engine then does with either is in [`takes_language`] (text-to-speech,
//! every family) and [`hears_language`] (speech-to-text, where the source
//! says more than the spec).

/// Families whose spec `languages` list *is* the vocabulary their engine
/// accepts, with the region a bare primary code means. Kokoro's languages
/// are `en-us`, `en-gb`, `fr-fr`, `pt-br`, … and a bare `en` is not one of
/// them (`kokoro_tts/session.cpp` validates the code). For every family not
/// listed, ISO codes in `languages` describe the model, not what its
/// request takes, and a client's code is sent as it came.
pub(crate) fn language_regions(family: &str) -> Option<&'static [(&'static str, &'static str)]> {
    match family {
        "kokoro_tts" => Some(&[("en", "en-us"), ("pt", "pt-br"), ("fr", "fr-fr")]),
        _ => None,
    }
}

/// What a text-to-speech family's engine does with a language — every
/// text-to-speech family audio.cpp has ([`takes_language`]). A family not
/// in the table (one audio.cpp adds later) is sent none: lmgw does not know
/// whether it takes one, and a code it refuses would fail every clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TakesLanguage {
    /// The request's `language`, exactly one of the spec's `languages` in
    /// their spelling; anything else is refused (or, for the last three,
    /// turned into something that is no language):
    /// - **Supertonic** validates against its 31 codes and wraps the text
    ///   in `<de>…</de>`; without a language every clause is `<en>…</en>`
    ///   (`supertonic/tokenizer_text.cpp` `preprocess`, the default of
    ///   `session.h`);
    /// - **Chatterbox** lowercases, keeps the primary subtag and looks it up
    ///   in its 19 codes (`chatterbox/text_tokenizer.cpp`
    ///   `normalize_chatterbox_language_code`);
    /// - **FireRedTTS3** takes the names its spec lists (`German`, …) as
    ///   `<|German|>` tags, `options.language` first, and speaks Chinese
    ///   without one (`fireredtts3/session.cpp` `request_language`,
    ///   `tokenizer_text.cpp` `is_supported_language`);
    /// - **Kitten TTS**, **Piper TTS** and **Inflect v2** speak English only
    ///   and refuse any other code (`kitten_tts/frontend.cpp`,
    ///   `piper_tts/session.cpp`, `inflect_v2/session.cpp`);
    /// - **MagpieTTS** normalises (`pt` → `pt-BR`) and refuses a language
    ///   its tokenizers lack (`magpie_tts/tokenizer_text.cpp`);
    /// - **Confucius4-TTS** looks the code up in its language tokens and
    ///   makes a Chinese instruction of anything else
    ///   (`confucius4_tts/tokenizer_text.cpp` `language_token`);
    /// - **FireRed Audio** (`options.language` first) speaks Chinese and
    ///   English (`firered_audio/session.cpp` `request_language`).
    SpecLanguages,
    /// The request's `language`, exactly a name of its package's own table:
    /// **Qwen3-TTS** looks the lowercased value up in `config.json`'s
    /// `talker_config.codec_language_id`, or `auto`, and refuses anything
    /// else (`qwen3_tts/talker.cpp`). Without that table read, nothing is
    /// sent.
    PackageTable,
    /// The request's `language` as an ISO code, which the engine reads
    /// itself and does not refuse for a code in use: **OmniVoice** looks it
    /// up in its 600+ ISO ids and names (`omnivoice/prompt_builder.cpp`
    /// `resolve_language`), **Dots TTS** tags the text with it uppercased
    /// (`dots_tts/session.cpp` `attach_language_tag`), **Audio8 TTS** and
    /// **VoxCPM1** read it only to keep Traditional Chinese for Cantonese
    /// (`audio8_tts/session.cpp`, `voxcpm1/session.cpp`).
    Code,
    /// The request's `language` as its English name: **MOSS-TTS Local**
    /// puts it in its prompt's `- Language:` slot as written
    /// (`moss/moss_tts_local/tokenizer_text.cpp`), the slot its MOSS
    /// siblings fill with a full name ("the model does not understand codes
    /// like 'en'"). Held against the spec's `languages`.
    Name,
    /// `options.language` only, as a code: **Sopro** (en, pt, fr, de,
    /// refusing others; `sopro_tts/text_tokenizer.cpp` `language_tag`),
    /// **Irodori-TTS** (`ja` only; `irodori_tts/session.cpp`), **IndexTTS2**
    /// (v2.5 reads any code, an unknown one as "common"; v2 ignores it —
    /// `index_tts2/session.cpp`, `tokenizer_text.cpp` `lang_to_id`). Held
    /// against the spec's `languages` where the spec declares the option.
    OptionCode,
    /// `options.language` only, as its English name: **MOSS-TTS v1.5**,
    /// **MOSS-TTSD** and **MOSS-VoiceGen** ("The model was trained on full
    /// language names; 'en' means nothing to it", their `session.cpp`).
    /// Held against the spec's `languages`.
    OptionName,
    /// The language comes from the voice: Kokoro resolves it from the voice
    /// id's first letter and refuses a request whose language differs
    /// (`kokoro_tts/frontend.cpp` `resolve_language_code`) — so a
    /// configured language is never sent to it ([`voice_language`]).
    Voice,
    /// One language per package, never a request's: **Pocket TTS** reads no
    /// `text_input.language` and refuses a style language
    /// (`pocket_tts/session.cpp`); its package's language is its weights'
    /// (the `language` load option only tunes defaults, `assets.cpp`).
    /// **SanoTTS** refuses any language but its voice package's
    /// (`sanotts/session.cpp`). [`package_language`] says where it is read.
    Package,
    /// Only `auto`: **VieNeu-TTS v3 Turbo** never fills its
    /// `codec_language_id`, so any other value is refused
    /// (`vieneu_v3_turbo/assets.cpp`, `talker.cpp`). It reads the text.
    AutoOnly,
    /// The engine never reads a request's language: what it speaks is the
    /// text, its package and its voice. Auk, BreezeTTS, Chatterbox Turbo,
    /// CosyVoice3, DramaBox, Echo-TTS, F5-TTS (its `dialect` is an Arabic
    /// dialect), Fish Audio S2, GLM-TTS, Higgs Audio TTS, MiniMax H3,
    /// MioTTS (a best-of-N scoring language only), Mira TTS, MOSS-TTS-Nano,
    /// NeuTTS, OuteTTS (the language of a reference clip only), Soprano,
    /// Vevo2, VibeVoice, VoxCPM2 and ZipVoice (`lang` is an eSpeak voice
    /// for its Latin-script runs).
    Nothing,
}

/// [`TakesLanguage`] for a text-to-speech `family`; `None` for a family not
/// in the table.
pub(crate) fn takes_language(family: &str) -> Option<TakesLanguage> {
    use TakesLanguage::*;
    Some(match family {
        "supertonic" | "chatterbox" | "fireredtts3" | "kitten_tts" | "piper_tts" | "inflect_v2"
        | "magpie_tts" | "confucius4_tts" | "firered_audio" => SpecLanguages,
        "qwen3_tts" => PackageTable,
        "omnivoice" | "dots_tts" | "audio8_tts" | "voxcpm1" => Code,
        "moss_tts_local" => Name,
        "sopro_tts" | "irodori_tts" | "index_tts2" => OptionCode,
        "moss_tts_v15" | "moss_ttsd" | "moss_voicegen" => OptionName,
        "kokoro_tts" => Voice,
        "pocket_tts" | "sanotts" => Package,
        "vieneu_v3_turbo" => AutoOnly,
        "auk" | "breeze_tts" | "chatterbox_turbo" | "cosyvoice3" | "dramabox" | "echo_tts"
        | "f5_tts" | "fish_audio" | "glm_tts" | "higgs_audio_tts" | "minimax_h3" | "miotts"
        | "mira_tts" | "moss_tts_nano" | "neutts" | "outetts" | "soprano_tts" | "vevo2"
        | "vibevoice" | "voxcpm2" | "zipvoice" => Nothing,
        _ => return None,
    })
}

/// What a speech-to-text family's engine does with a language, where its
/// source says more than its spec. A family not listed is sent the code in
/// its vocabulary's spelling where it declares one, else as it came; where
/// its spec declares a `language` option and lists codes, a code outside
/// them is noted (Granite 5 ASR and Niagara declare English and read none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HearsLanguage {
    /// It detects the language and reads none: **Parakeet TDT** reads only
    /// `keep_language_tags` (`parakeet_tdt/session.cpp`).
    Detects,
    /// It refuses a language outside its set: **Nemotron ASR**'s package
    /// prompts ("unsupported language prompt", `nemotron_asr/session.cpp`
    /// `prompt_id_for_request`), and the spec's `languages` of **Canary**
    /// (en, de, es, fr; `canary_asr/session.cpp`), **Cohere Transcribe**
    /// (`cohere_asr/session.cpp`), **Hviske** (`hviske_asr/session.cpp`),
    /// **Confucius4 R2T2** (`confucius4_r2t2/session.cpp`) and
    /// **Fun-ASR-Nano** (auto, zh, en, ja; `fun_asr_nano/prompt.cpp`).
    RefusesOthers,
    /// Only its package's language, refusing any other: **Kroko ASR**
    /// (`kroko_asr/session.cpp` `request_language`, the package's
    /// `config.json` `language.iso`).
    Package,
    /// It reads only these, and transcribes anything else as `auto`:
    /// **SenseVoice**'s language query tokens (`sense_asr/session.cpp`
    /// `language_query_token`) — far fewer than the languages its spec
    /// lists, which it detects.
    Only(&'static [&'static str]),
}

/// [`HearsLanguage`] for a speech-to-text `family`, where its source says.
pub(crate) fn hears_language(family: &str) -> Option<HearsLanguage> {
    use HearsLanguage::*;
    Some(match family {
        "parakeet_tdt" => Detects,
        "nemotron_asr" | "canary_asr" | "cohere_asr" | "hviske_asr" | "confucius4_r2t2"
        | "fun_asr_nano" => RefusesOthers,
        "kroko_asr" => Package,
        "sense_asr" => Only(&["auto", "zh", "en", "yue", "ja", "ko"]),
        _ => return None,
    })
}

/// Where a family with one language per package says which
/// ([`TakesLanguage::Package`], [`HearsLanguage::Package`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageLanguage {
    /// Pocket TTS: the row's `language` load option (`german`,
    /// `english_2026-04`, `french_24l`), else the package's own name
    /// (`PocketTTS-GGUF/german`, `pocket-tts-german-q8_0.gguf`). audio.cpp
    /// loads a package without the option as `english`, which only tunes
    /// defaults: the weights speak what they were trained on.
    LoadOptionOrName,
    /// SanoTTS: the package's `config.json` — `graph` `nano` (or none)
    /// speaks English, `piperlite` its `language` (`sanotts/assets.cpp`,
    /// `session.cpp`).
    SanoConfig,
    /// Kroko ASR: the package's `config.json` `language.iso`
    /// (`kroko_asr/assets.cpp`).
    KrokoConfig,
}

/// [`PackageLanguage`] for `family`; `None` for a family whose language is
/// not its package's.
pub(crate) fn package_language(family: &str) -> Option<PackageLanguage> {
    match family {
        "pocket_tts" => Some(PackageLanguage::LoadOptionOrName),
        "sanotts" => Some(PackageLanguage::SanoConfig),
        "kroko_asr" => Some(PackageLanguage::KrokoConfig),
        _ => None,
    }
}

/// The ISO 639-1 code a voice of a [`TakesLanguage::Voice`] family speaks:
/// Kokoro's voice ids start with a letter for their language (`af_heart`,
/// `bm_george`, `ef_dora`; `kokoro_tts/frontend.cpp`
/// `resolve_language_code_alias`). `None` for another family or an id it
/// does not read as a voice.
pub(crate) fn voice_language(family: &str, voice: &str) -> Option<&'static str> {
    if family != "kokoro_tts" {
        return None;
    }
    let mut chars = voice.chars();
    let (first, second) = (chars.next()?, chars.next()?);
    if !matches!(second, 'f' | 'm') {
        return None;
    }
    Some(match first {
        'a' | 'b' => "en",
        'e' => "es",
        'f' => "fr",
        'h' => "hi",
        'i' => "it",
        'j' => "ja",
        'p' => "pt",
        'z' => "zh",
        _ => return None,
    })
}

/// Families whose package embeds a `processor_config.json` with a
/// `prompt_dictionary` — the exact language prompts the engine looks a
/// transcription's `language` up in, refusing anything else with "Nemotron
/// ASR unsupported language prompt" (`nemotron_asr/session.cpp`
/// `prompt_id_for_request`). Its spec's `languages` (`de-DE`, `en-US`, …)
/// are a subset of the keys: the published package also has `de`, `en`,
/// `auto` and more, and lacks bare `ja` or `zh`.
pub(crate) fn reads_prompt_dictionary(family: &str) -> bool {
    family == "nemotron_asr"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every text-to-speech family of audio.cpp's specs at 94bd465 (a
    /// `tts`, `clone` or `design` task) is in the table: a family missing
    /// from it is sent no language.
    #[test]
    fn every_text_to_speech_family_of_the_audited_build_is_listed() {
        for family in [
            "audio8_tts",
            "auk",
            "breeze_tts",
            "chatterbox",
            "chatterbox_turbo",
            "confucius4_tts",
            "cosyvoice3",
            "dots_tts",
            "dramabox",
            "echo_tts",
            "f5_tts",
            "firered_audio",
            "fireredtts3",
            "fish_audio",
            "glm_tts",
            "higgs_audio_tts",
            "index_tts2",
            "inflect_v2",
            "irodori_tts",
            "kitten_tts",
            "kokoro_tts",
            "magpie_tts",
            "minimax_h3",
            "miotts",
            "mira_tts",
            "moss_tts_local",
            "moss_tts_nano",
            "moss_tts_v15",
            "moss_ttsd",
            "moss_voicegen",
            "neutts",
            "omnivoice",
            "outetts",
            "piper_tts",
            "pocket_tts",
            "qwen3_tts",
            "sanotts",
            "soprano_tts",
            "sopro_tts",
            "supertonic",
            "vevo2",
            "vibevoice",
            "vieneu_v3_turbo",
            "voxcpm1",
            "voxcpm2",
            "zipvoice",
        ] {
            assert!(takes_language(family).is_some(), "{family}");
        }
        assert_eq!(takes_language("future_tts"), None);
    }
}
