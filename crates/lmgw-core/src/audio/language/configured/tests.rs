//! The configured language against profiles shaped like the owner's rows
//! (read off the catalog and the packages: Supertonic-3, Qwen3-TTS
//! CustomVoice, Kokoro, MagpieTTS, Pocket TTS, Nemotron ASR, Parakeet TDT)
//! and like the families the table sorts otherwise (Sopro, MOSS-TTS v1.5,
//! IndexTTS2, VieNeu-TTS, SanoTTS, Canary, SenseVoice, Kroko).

use super::*;
use crate::audio::profile::LanguageVocab;

fn vocab(source: VocabSource, entries: &[&str]) -> LanguageVocab {
    LanguageVocab {
        source,
        entries: entries.iter().map(|e| e.to_string()).collect(),
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|e| e.to_string()).collect()
}

fn supertonic() -> SpeechProfile {
    SpeechProfile {
        family: "supertonic".into(),
        language_vocab: vocab(VocabSource::SpecRequest, &["en", "ko", "de", "fr"]),
        spec_languages: strings(&["en", "ko", "de", "fr"]),
        ..Default::default()
    }
}

#[test]
fn supertonic_is_sent_the_configured_code_and_a_hint_stays_a_hint() {
    let p = supertonic();
    let fit = tts_fit("de", &p, Some("M5"));
    assert_eq!(fit.send.as_deref(), Some("de"));
    assert_eq!(fit.note, None);
    assert_eq!(
        SpeechLanguage::Request("de".into()).for_row(&p, None),
        Some((Field::Language, "de".to_string()))
    );
    assert_eq!(
        SpeechLanguage::Hint("de".into()).for_row(&p, None),
        None,
        "a session's transcription language is not the reply's"
    );
}

#[test]
fn a_known_set_without_the_code_gets_no_language_and_a_note() {
    let fit = tts_fit("sw", &supertonic(), None);
    assert_eq!(fit.send, None);
    let note = fit.note.expect("a note");
    assert!(note.starts_with("does not speak sw (sw)"), "{note}");
    assert!(note.contains("de, en, fr, ko"), "{note}");

    let qwen3 = SpeechProfile {
        family: "qwen3_tts".into(),
        language_vocab: vocab(VocabSource::Gguf, &["english", "german", "auto"]),
        ..Default::default()
    };
    assert_eq!(tts_fit("de", &qwen3, None).send.as_deref(), Some("german"));
    let fit = tts_fit("sv", &qwen3, None);
    assert_eq!(fit.send, None);
    assert!(
        fit.note.unwrap().starts_with("does not speak Swedish (sv)"),
        "named by its English name"
    );

    let magpie = SpeechProfile {
        family: "magpie_tts".into(),
        language_option: true,
        language_vocab: vocab(VocabSource::SpecRequest, &["de", "en", "pt-BR"]),
        spec_languages: strings(&["de", "en", "pt-BR"]),
        ..Default::default()
    };
    assert_eq!(tts_fit("pt", &magpie, None).send.as_deref(), Some("pt-BR"));
    assert_eq!(tts_fit("sv", &magpie, None).send, None);
}

#[test]
fn fireredtts3_gets_its_own_name_for_the_code() {
    let p = SpeechProfile {
        family: "fireredtts3".into(),
        language_option: true,
        language_vocab: vocab(VocabSource::SpecRequest, &["Chinese", "English", "German"]),
        ..Default::default()
    };
    assert_eq!(tts_fit("de", &p, None).send.as_deref(), Some("German"));
    // And a stock session's hint reaches it as before the conversation
    // language: through its `language` option.
    assert_eq!(
        SpeechLanguage::Hint("de".into()).for_row(&p, None),
        Some((Field::Language, "German".to_string()))
    );
}

#[test]
fn kokoro_is_never_sent_one_and_a_voice_of_another_language_is_said() {
    let p = SpeechProfile {
        family: "kokoro_tts".into(),
        language_option: true,
        language_vocab: vocab(VocabSource::FamilyTable, &["en-us", "en-gb", "es"]),
        ..Default::default()
    };
    let fit = tts_fit("de", &p, Some("af_heart"));
    assert_eq!(fit.send, None);
    let note = fit.note.expect("the voice speaks English");
    assert!(note.contains("'af_heart' speaks English"), "{note}");
    assert!(note.contains("(German)"), "{note}");
    let same = tts_fit("en", &p, Some("bf_emma"));
    assert_eq!(same, Fit::default(), "its voice speaks it: nothing to say");
    assert_eq!(
        SpeechLanguage::Request("es".into()).for_row(&p, Some("ef_dora")),
        None,
        "its voice decides, even when they agree"
    );
}

#[test]
fn an_engine_that_reads_no_language_is_said_to() {
    let p = SpeechProfile {
        family: "fish_audio".into(),
        spec_languages: strings(&["80+ languages"]),
        ..Default::default()
    };
    let fit = tts_fit("de", &p, Some("alba"));
    assert_eq!(fit.send, None);
    assert!(fit.note.unwrap().starts_with("takes no language"));

    // VieNeu-TTS takes `auto` alone: anything else would fail every clause.
    let vieneu = SpeechProfile {
        family: "vieneu_v3_turbo".into(),
        spec_languages: strings(&["vi", "en"]),
        ..Default::default()
    };
    let fit = tts_fit("vi", &vieneu, None);
    assert_eq!(fit.send, None);
    assert!(fit
        .note
        .unwrap()
        .starts_with("takes no language but `auto`"));
}

#[test]
fn a_package_of_one_language_is_noted_only_when_it_speaks_another() {
    let pocket = |lang: Option<&str>| SpeechProfile {
        family: "pocket_tts".into(),
        spec_languages: strings(&["en", "de", "it", "pt", "es"]),
        package_language: lang.map(str::to_string),
        ..Default::default()
    };
    assert_eq!(
        tts_fit("de", &pocket(Some("de")), Some("alba")),
        Fit::default(),
        "the German package speaks German: nothing sent, nothing to say"
    );
    let fit = tts_fit("de", &pocket(Some("en")), Some("alba"));
    assert_eq!(fit.send, None);
    let note = fit.note.unwrap();
    assert!(
        note.starts_with("speaks only its package's language, English"),
        "{note}"
    );
    let fit = tts_fit("de", &pocket(None), None);
    assert!(fit.note.unwrap().contains("which lmgw cannot tell"));

    let sano = SpeechProfile {
        family: "sanotts".into(),
        package_language: Some("de".into()),
        ..Default::default()
    };
    assert_eq!(tts_fit("de", &sano, None), Fit::default());
    assert!(tts_fit("en", &sano, None).note.unwrap().contains("German"));
}

#[test]
fn a_family_lmgw_does_not_know_is_sent_none() {
    let p = SpeechProfile {
        family: "future_tts".into(),
        spec_languages: strings(&["de", "en"]),
        language_option: true,
        ..Default::default()
    };
    let fit = tts_fit("de", &p, None);
    assert_eq!(fit.send, None);
    assert_eq!(
        fit.note.as_deref(),
        Some(
            "is a future_tts row: lmgw does not know whether future_tts takes a language; not \
             sent"
        )
    );
}

#[test]
fn qwen3_without_its_table_is_sent_none() {
    let p = SpeechProfile {
        family: "qwen3_tts".into(),
        spec_languages: strings(&["zh", "en", "de"]),
        ..Default::default()
    };
    let fit = tts_fit("de", &p, None);
    assert_eq!(fit.send, None, "`de` is no name its talker takes");
    assert!(
        fit.note.unwrap().contains("lmgw could not read them"),
        "said"
    );
}

#[test]
fn an_option_only_family_is_sent_options_language_in_its_spelling() {
    // Sopro reads `options.language` alone, and refuses a code it lacks.
    let sopro = SpeechProfile {
        family: "sopro_tts".into(),
        language_option: true,
        spec_languages: strings(&["en", "pt", "fr", "de"]),
        ..Default::default()
    };
    let fit = tts_fit("de", &sopro, None);
    assert_eq!(
        (fit.send.as_deref(), fit.field),
        (Some("de"), Field::Options)
    );
    assert_eq!(tts_fit("sv", &sopro, None).send, None);
    assert_eq!(
        SpeechLanguage::Request("de".into()).for_row(&sopro, None),
        Some((Field::Options, "de".to_string()))
    );
    // A stock session's hint goes where it always went.
    assert_eq!(
        SpeechLanguage::Hint("de".into()).for_row(&sopro, None),
        Some((Field::Language, "de".to_string()))
    );
    // MOSS-TTS v1.5 wants the full name.
    let moss = SpeechProfile {
        family: "moss_tts_v15".into(),
        language_option: true,
        spec_languages: strings(&["en", "zh"]),
        ..Default::default()
    };
    let fit = tts_fit("en", &moss, None);
    assert_eq!(
        (fit.send.as_deref(), fit.field),
        (Some("English"), Field::Options)
    );
    assert!(tts_fit("de", &moss, None)
        .note
        .unwrap()
        .starts_with("does not speak German (de)"));
    // IndexTTS2 declares no option and reads any code (v2.5).
    let index = SpeechProfile {
        family: "index_tts2".into(),
        spec_languages: strings(&["zh", "en"]),
        ..Default::default()
    };
    let fit = tts_fit("de", &index, None);
    assert_eq!(
        (fit.send.as_deref(), fit.field),
        (Some("de"), Field::Options)
    );
}

#[test]
fn a_family_that_reads_any_code_is_sent_it_as_it_came() {
    let p = SpeechProfile {
        family: "omnivoice".into(),
        spec_languages: strings(&["600+ languages"]),
        ..Default::default()
    };
    assert_eq!(
        tts_fit("de", &p, None),
        Fit::sent("de".into(), Field::Language)
    );
    assert_eq!(tts_fit("  ", &p, None), Fit::default());
}

#[test]
fn asr_maps_into_nemotron_s_prompts_and_parakeet_is_said_to_detect() {
    let nemotron = SpeechProfile {
        family: "nemotron_asr".into(),
        language_vocab: vocab(
            VocabSource::Gguf,
            &["auto", "de", "de-DE", "id-ID", "ja-JA", "ja-JP"],
        ),
        ..Default::default()
    };
    assert_eq!(asr_fit("de", &nemotron).send.as_deref(), Some("de"));
    assert_eq!(asr_fit("id", &nemotron).send.as_deref(), Some("id-ID"));
    let fit = asr_fit("ja", &nemotron);
    assert_eq!(fit.send.as_deref(), Some("ja"), "sent as it came");
    assert!(fit
        .note
        .unwrap()
        .starts_with("has no prompt for Japanese (ja)"));

    let parakeet = SpeechProfile {
        family: "parakeet_tdt".into(),
        spec_languages: strings(&["de", "en"]),
        ..Default::default()
    };
    let fit = asr_fit("de", &parakeet);
    assert_eq!(fit.send.as_deref(), Some("de"), "harmless: never read");
    assert!(fit.note.unwrap().starts_with("detects the language itself"));

    let qwen3 = SpeechProfile {
        family: "qwen3_asr".into(),
        spec_languages: strings(&["zh", "en", "de"]),
        ..Default::default()
    };
    assert_eq!(
        asr_fit("de", &qwen3),
        Fit::sent("de".into(), Field::Language)
    );
}

#[test]
fn asr_with_a_known_set_says_when_the_language_is_outside_it() {
    // Canary refuses a language outside en, de, es, fr.
    let canary = SpeechProfile {
        family: "canary_asr".into(),
        language_option: true,
        spec_languages: strings(&["en", "de", "es", "fr"]),
        ..Default::default()
    };
    assert_eq!(
        asr_fit("de", &canary),
        Fit::sent("de".into(), Field::Language)
    );
    let fit = asr_fit("it", &canary);
    assert_eq!(fit.send.as_deref(), Some("it"), "the engine is the judge");
    let note = fit.note.unwrap();
    assert!(
        note.starts_with(
            "takes only de, en, es, fr as a language, not Italian (it), so it refuses"
        ),
        "{note}"
    );
    // Granite 5 declares the option and reads none: no refusal claimed.
    let granite = SpeechProfile {
        family: "granite5asr".into(),
        language_option: true,
        spec_languages: strings(&["en"]),
        ..Default::default()
    };
    let note = asr_fit("de", &granite).note.unwrap();
    assert_eq!(
        note,
        "takes only en as a language: the language setting (German) does not reach it"
    );
    // SenseVoice reads only its query tokens; anything else is `auto`.
    let sense = SpeechProfile {
        family: "sense_asr".into(),
        language_option: true,
        spec_languages: strings(&["auto", "zh", "en", "de"]),
        ..Default::default()
    };
    assert_eq!(asr_fit("en", &sense).note, None);
    assert!(asr_fit("de", &sense)
        .note
        .unwrap()
        .contains("it detects the language itself"));
    // Kroko hears its package's language only.
    let kroko = SpeechProfile {
        family: "kroko_asr".into(),
        language_option: true,
        spec_languages: strings(&["de", "en"]),
        package_language: Some("en".into()),
        ..Default::default()
    };
    assert_eq!(asr_fit("en", &kroko).note, None);
    assert!(asr_fit("de", &kroko)
        .note
        .unwrap()
        .starts_with("hears only its package's language, English"));
}
