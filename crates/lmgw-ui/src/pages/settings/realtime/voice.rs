//! The default voice: a free box, offered the voices of the TTS model the
//! draft names ([`crate::widgets::voice_picker`]).

use leptos::prelude::*;

use super::super::{Def, Page};
use crate::widgets::voice_picker::{use_voices, VoiceInput};
use crate::widgets::Field;

/// The TTS model the draft names: the first of `keys` that is set — the
/// Chat's own alias falls back to realtime's, as the gateway resolves it.
pub(in crate::pages::settings) fn drafted_tts(page: Page, keys: &'static [&'static str]) -> String {
    keys.iter()
        .map(|k| page.form.text(k).trim().to_string())
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}

/// What an empty `chat_voice` inherits as the drafts stand: realtime's
/// default voice, when there is one and the Chat speaks with realtime's
/// model (its own TTS empty, or the same alias); `None` otherwise.
fn inherited_voice(chat_tts: &str, rt_tts: &str, rt_voice: &str) -> Option<String> {
    let (chat_tts, rt_tts, rt_voice) = (chat_tts.trim(), rt_tts.trim(), rt_voice.trim());
    (!rt_voice.is_empty() && (chat_tts.is_empty() || chat_tts == rt_tts))
        .then(|| rt_voice.to_string())
}

/// A voice setting: a free box (any name the model takes), offered the
/// voices of the TTS model the draft names in `tts_keys`.
#[component]
pub(in crate::pages::settings) fn VoiceField(
    d: &'static Def,
    page: Page,
    dirty: Signal<bool>,
    error: Signal<Option<String>>,
    id: String,
    hidden: Signal<bool>,
    tts_keys: &'static [&'static str],
) -> impl IntoView {
    let form = page.form;
    let k = d.key;
    // Asked when the box is focused, not on every Settings visit.
    let list = use_voices(Signal::derive(move || drafted_tts(page, tts_keys)));
    let voices = list.voices;
    // An empty Chat voice leaves realtime's chain to decide, which starts
    // from realtime's default voice — chosen for realtime's model, so the
    // box says so only while the Chat speaks with that model (chat-voice
    // §2.3).
    let placeholder = Signal::derive(move || {
        (k == "chat_voice")
            .then(|| {
                inherited_voice(
                    &form.text("chat_tts_alias"),
                    &form.text("realtime.tts_alias"),
                    &form.text("realtime.default_voice"),
                )
            })
            .flatten()
            .map(|v| format!("inherits {v}"))
            .or_else(|| voices.get().default_voice())
            .unwrap_or_default()
    });
    let list_id = format!("{id}-voices");
    view! {
        <Field
            label=d.label
            unit=d.unit
            hint=d.hint
            dirty=dirty
            error=error
            id=id
            hidden=hidden
        >
            <VoiceInput
                value=Signal::derive(move || form.text(k))
                on_input=Callback::new(move |v: String| form.set_text(k, v))
                list=list
                list_id=list_id
                placeholder=placeholder
            />
        </Field>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chat_inherits_realtimes_voice_only_with_realtimes_model() {
        assert_eq!(inherited_voice("", "rt", "M5").as_deref(), Some("M5"));
        assert_eq!(inherited_voice(" rt ", "rt", "M5").as_deref(), Some("M5"));
        assert_eq!(inherited_voice("other", "rt", "M5"), None);
        assert_eq!(inherited_voice("", "rt", " "), None);
    }
}
