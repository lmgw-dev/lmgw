//! A thread's two languages in its Voice section (chat-voice design §2.1,
//! split 2026-10-05): "I speak" (`voice.language`), what speech recognition
//! is told, and "Replies in" (`voice.reply_language`), what the model
//! answers in and the voice speaks. Each says what is in effect and where it
//! comes from — the gateway's resolution, a reply that follows the spoken
//! language included — and, beneath it, where its stage's model does not
//! take it (the ASR's notes under the one, the TTS's under the other).
//!
//! `auto` on either is the thread's own "none": on "I speak" none whatever
//! Settings say (the ASR detects); on "Replies in" not Settings' reply
//! language — the language spoken, as resolved.

use leptos::prelude::*;

use super::{source_label, Sourced, VoiceResolved};

/// The spoken language's "in effect" line.
pub(super) fn spoken_line(l: &Sourced<Option<String>>) -> String {
    match (&l.value, l.source.as_deref()) {
        (Some(v), src) => format!("in effect: {v} · {}", source_label(src)),
        (None, Some(src)) => format!(
            "in effect: auto — speech recognition detects · {}",
            source_label(Some(src))
        ),
        (None, None) => "in effect: none — speech recognition detects".to_string(),
    }
}

/// The reply language's "in effect" line.
pub(super) fn reply_line(l: &Sourced<Option<String>>) -> String {
    match &l.value {
        Some(v) => format!("in effect: {v} · {}", source_label(l.source.as_deref())),
        None => "in effect: none — the reply follows you".to_string(),
    }
}

/// The reply box's placeholder while it is empty: what it would inherit,
/// as the saved resolution says — Settings' reply language, or the spoken
/// language it follows. A folder (no resolution) says the chain.
pub(super) fn reply_placeholder(r: Option<&VoiceResolved>) -> String {
    match r.map(|r| (&r.reply_language.value, r.reply_language.source.as_deref())) {
        Some((Some(v), Some("chat"))) => format!("inherit: {v}"),
        Some((_, Some("speech_in") | None)) => "as I speak".to_string(),
        _ => "inherit, else as I speak".to_string(),
    }
}

/// The two boxes, side by side, each with its in-effect line and its
/// stage's notes. `resolved`: the thread's `voice_resolved`; `None` for a
/// folder's defaults.
#[component]
pub(super) fn LanguageFields(
    language: RwSignal<String>,
    reply: RwSignal<String>,
    resolved: Option<Signal<Option<VoiceResolved>>>,
) -> impl IntoView {
    let now = move || resolved.and_then(|r| r.get());
    let notes = move |stage: &'static str| {
        move || {
            now()
                .map(|r| r.language_notes)
                .unwrap_or_default()
                .into_iter()
                .filter(|n| n.stage == stage)
                .map(|n| view! { <div class="field-hint">{n.message}</div> })
                .collect_view()
        }
    };
    view! {
        <div class="field-grid" style="--field-min:160px">
            <div class="field" data-voice-language="">
                <label title="the language you speak: speech recognition is told it">"I speak"</label>
                <input
                    class="input mono"
                    placeholder="inherit"
                    maxlength="4"
                    title="two letters (de, en), or auto: none here — speech recognition detects"
                    prop:value=move || language.get()
                    on:input=move |ev| language.set(event_target_value(&ev))
                />
                {move || now().map(|r| view! { <div class="field-hint">{spoken_line(&r.language)}</div> })}
                {notes("asr")}
            </div>
            <div class="field" data-voice-reply-language="">
                <label title="the language the model answers in and the voice speaks">"Replies in"</label>
                <input
                    class="input mono"
                    placeholder=move || reply_placeholder(now().as_ref())
                    maxlength="4"
                    title="two letters (de, en), or auto: the language you speak, whatever Settings say; empty inherits"
                    prop:value=move || reply.get()
                    on:input=move |ev| reply.set(event_target_value(&ev))
                />
                {move || now().map(|r| view! { <div class="field-hint">{reply_line(&r.reply_language)}</div> })}
                {notes("tts")}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sourced(value: Option<&str>, source: Option<&str>) -> Sourced<Option<String>> {
        Sourced {
            value: value.map(str::to_string),
            source: source.map(str::to_string),
        }
    }

    #[test]
    fn each_line_says_what_is_in_effect_and_where_from() {
        assert_eq!(
            spoken_line(&sourced(Some("de"), Some("thread"))),
            "in effect: de · this thread"
        );
        assert_eq!(
            spoken_line(&sourced(None, Some("thread"))),
            "in effect: auto — speech recognition detects · this thread"
        );
        assert_eq!(
            spoken_line(&sourced(None, None)),
            "in effect: none — speech recognition detects"
        );
        assert_eq!(
            reply_line(&sourced(Some("de"), Some("speech_in"))),
            "in effect: de · the language you speak"
        );
        assert_eq!(
            reply_line(&sourced(Some("en"), Some("chat"))),
            "in effect: en · Settings → Chat"
        );
        assert_eq!(
            reply_line(&sourced(None, None)),
            "in effect: none — the reply follows you"
        );
    }

    #[test]
    fn the_reply_placeholder_names_what_an_empty_box_inherits() {
        let with = |v: Option<&str>, s: Option<&str>| VoiceResolved {
            reply_language: sourced(v, s),
            ..Default::default()
        };
        assert_eq!(
            reply_placeholder(Some(&with(Some("en"), Some("chat")))),
            "inherit: en"
        );
        assert_eq!(
            reply_placeholder(Some(&with(Some("de"), Some("speech_in")))),
            "as I speak"
        );
        assert_eq!(reply_placeholder(Some(&with(None, None))), "as I speak");
        assert_eq!(reply_placeholder(None), "inherit, else as I speak");
    }
}
