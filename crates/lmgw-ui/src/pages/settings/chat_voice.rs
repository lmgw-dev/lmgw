//! Settings → Chat → Voice (chat-voice design §2.1): the speech models, the
//! voice and its style, the two languages, read-aloud, turn
//! detection and audio input (voice-audio-input design §2.1) a Chat thread
//! uses unless it overrides them.
//!
//! Two languages (split 2026-10-05): the one the user speaks ("I speak")
//! goes to speech recognition where the model takes a language; the one
//! replies are in ("Replies in") is what the model answers in and the voice
//! pronounces — empty, it is the spoken one, so one language set drives all
//! three stages. Each field says, from the saved settings, where its
//! stage's model does not take it ([`LanguageField`], the server's
//! `chat_voice_language_notes`).
//!
//! Each "empty" falls through to Settings → Realtime, as the gateway
//! resolves it: the Chat's transcription model to realtime's, and back again
//! (realtime prefers its own), so one of them configured serves both. The
//! pickers are realtime's own (task-filtered models, the voice list of the
//! drafted TTS, the style told what that TTS does with it).

use leptos::prelude::*;
use lmgw_api_types::chat_voice::{AUDIO_INPUTS, TURN_DETECTIONS};

use super::{f, Ctl, Def, Page};
use crate::widgets::Field;

/// The Chat's TTS model, then realtime's: what the voice and style fields
/// read their model from.
const CHAT_TTS: &[&str] = &["chat_tts_alias", "realtime.tts_alias"];

/// The group's rows, in page order.
pub(super) const CHAT_VOICE: &[Def] = &[
    f(
        "chat",
        "chat-voice",
        "chat_stt_alias",
        "Speech to text",
        Ctl::Model(&["asr"], None, "the Realtime speech-to-text model"),
    )
    .hint(
        "dictation, voice mode, and the transcript of an audio attachment the chat model \
         cannot hear; a thread can override it",
    )
    .terms("chat audio stt speech transcription whisper asr attachment dictation voice"),
    f(
        "chat",
        "chat-voice",
        "chat_tts_alias",
        "Text to speech",
        Ctl::Model(
            lmgw_api_types::realtime::SPEECH_TASKS,
            None,
            "the Realtime text-to-speech model",
        ),
    )
    .hint("reads replies aloud and speaks in voice mode; a thread can override it")
    .terms("chat tts speech synthesis read aloud voice"),
    f(
        "chat",
        "chat-voice",
        "chat_voice",
        "Voice",
        Ctl::Voice(CHAT_TTS),
    )
    .hint(
        "a voice of the Chat's text-to-speech model; empty = the Realtime voice (when the Chat \
         speaks with the Realtime model), then the model's default",
    )
    .terms("chat voice speaker preset"),
    f(
        "chat",
        "chat-voice",
        "chat_speech_style",
        "Speech style",
        Ctl::SpeechStyle(CHAT_TTS),
    )
    .ph("calm, warm, unhurried")
    .hint("what the voice is told; empty = the Realtime speech style; a thread can say none")
    .terms("chat speech instructions style tone voice design description"),
    f(
        "chat",
        "chat-voice",
        "chat_voice_language",
        "I speak",
        Ctl::VoiceLanguage,
    )
    .ph("de")
    .unit("ISO 639-1")
    .hint(
        "the language you speak: speech recognition hears it where the model takes a language; \
         empty = it detects; replies are in it too unless Replies in says otherwise; a thread \
         can override it",
    )
    .terms("chat voice language spoken speak input asr locale german english"),
    f(
        "chat",
        "chat-voice",
        "chat_voice_reply_language",
        "Replies in",
        Ctl::VoiceLanguage,
    )
    .ph("as I speak")
    .unit("ISO 639-1")
    .hint(
        "the language the model answers in and the voice speaks; empty = the language you speak \
         (with neither, the reply follows you); a thread can override it",
    )
    .terms("chat voice reply answer output language tts locale german english"),
    f(
        "chat",
        "chat-voice",
        "chat_read_aloud",
        "Read replies aloud",
        Ctl::Bool,
    )
    .hint("as they stream; a thread can override it")
    .terms("chat read aloud speak tts autoplay"),
    f(
        "chat",
        "chat-voice",
        "chat_turn_detection",
        "Turn detection in voice mode",
        Ctl::Choice(TURN_DETECTIONS),
    )
    .hint("when a spoken turn ends; a thread can override it")
    .terms("chat voice realtime vad smart turn push to talk ptt"),
    f(
        "chat",
        "chat-voice",
        "chat_voice_audio_input",
        "Audio input in voice mode",
        Ctl::Choice(AUDIO_INPUTS),
    )
    .hint(
        "experimental: a voice turn goes to the chat model as audio when the model that answers \
         it takes audio input. A configured fallback is always used: one that takes audio hears \
         the turn wherever it runs. A model lmgw cannot judge reads the transcript until its \
         alias's capabilities override says task chat and input modalities text and audio (a \
         passthrough model needs an alias). Speech to text still transcribes every turn either \
         way, so a cloud speech-to-text model still receives the audio (the transcript is what \
         is saved); a thread can override it",
    )
    .terms("chat voice audio input hear omni multimodal gemma fallback cloud experimental"),
];

/// A language box, and beneath it where the saved settings' speech model of
/// its stage does not take the saved language (`chat_voice_language_notes`:
/// under "I speak" an ASR that detects the language itself, under "Replies
/// in" a voice that cannot speak it). Read back after every save, so it
/// follows what is stored, not the draft. The reply box's placeholder names
/// the spoken language it follows while it is empty.
#[component]
pub(super) fn LanguageField(
    d: &'static Def,
    page: Page,
    dirty: Signal<bool>,
    error: Signal<Option<String>>,
    warn: Signal<Option<String>>,
    id: String,
    hidden: Signal<bool>,
) -> impl IntoView {
    let form = page.form;
    let k = d.key;
    let stage = stage_of(k);
    let placeholder = move || match k {
        "chat_voice_reply_language" => follows(&form.text("chat_voice_language")),
        _ => d.ph.to_string(),
    };
    let notes = move || -> Vec<String> {
        page.data.with(|v| {
            v.as_ref()
                .and_then(|v| {
                    serde_json::from_value::<Vec<lmgw_api_types::chat_voice::LanguageNote>>(
                        v["chat_voice_language_notes"].clone(),
                    )
                    .ok()
                })
                .unwrap_or_default()
                .into_iter()
                .filter(|n| n.stage == stage)
                .map(|n| n.message)
                .collect()
        })
    };
    view! {
        <Field
            label=d.label
            unit=d.unit
            hint=d.hint
            dirty=dirty
            error=error
            warn=warn
            id=id
            hidden=hidden
        >
            <input
                class="input mono"
                placeholder=placeholder
                spellcheck="false"
                prop:value=move || form.text(k)
                on:input=move |ev| form.set_text(k, event_target_value(&ev))
            />
            {move || {
                notes()
                    .into_iter()
                    .map(|m| view! { <div class="field-hint">{m}</div> })
                    .collect_view()
            }}
        </Field>
    }
}

/// The stage whose notes go under language box `key`: the spoken
/// language's is the ASR, the reply's the TTS.
fn stage_of(key: &str) -> &'static str {
    match key {
        "chat_voice_reply_language" => "tts",
        _ => "asr",
    }
}

/// The reply box's placeholder while it is empty: the spoken language it
/// follows, as drafted.
fn follows(spoken: &str) -> String {
    match spoken.trim() {
        "" => "as I speak".to_string(),
        l => format!("as I speak: {}", l.to_ascii_lowercase()),
    }
}

/// What the server would refuse in this group, said before Save: a
/// language that is not two letters.
pub(super) fn error(key: &str, text: &str) -> Option<String> {
    let empty = match key {
        "chat_voice_language" => "or empty for none",
        "chat_voice_reply_language" => "or empty for the language you speak",
        _ => return None,
    };
    lmgw_api_types::chat_voice::language_hint(text)
        .is_none()
        .then(|| format!("two letters (ISO 639-1, such as de or en), {empty}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_language_is_two_letters_or_empty() {
        for ok in ["", "de", "EN", " fr "] {
            assert_eq!(error("chat_voice_language", ok), None, "{ok:?}");
        }
        for bad in ["deu", "d", "d1", "de-AT"] {
            assert!(error("chat_voice_language", bad).is_some(), "{bad}");
        }
        assert_eq!(error("chat_voice", "anything at all"), None);
        assert_eq!(error("chat_voice_reply_language", "En"), None);
        let e = error("chat_voice_reply_language", "auto").unwrap();
        assert!(e.ends_with("the language you speak"), "{e}");
    }

    #[test]
    fn the_reply_box_names_what_it_follows_and_each_box_its_stage() {
        assert_eq!(follows(""), "as I speak");
        assert_eq!(follows(" DE "), "as I speak: de");
        assert_eq!(stage_of("chat_voice_language"), "asr");
        assert_eq!(stage_of("chat_voice_reply_language"), "tts");
    }
}
