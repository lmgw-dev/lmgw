//! The Voice section of a thread's settings (chat-voice design §2.2): the
//! speech models, the voice, read-aloud, turn detection, the two languages
//! ([`super::languages`]), audio
//! input (voice-audio-input design §2.3) and the speech style a thread may
//! set for itself. Empty inherits.
//!
//! Beside each field the thread shows what is in effect and where it comes
//! from (`voice_resolved`, §2.3) — the gateway's own resolution, never
//! re-derived here. A folder's defaults form has no thread to resolve: it
//! shows the fields only, and its voice list follows the model a new thread
//! would inherit (Settings → Chat, then Settings → Realtime).
//!
//! Nothing here asks a model for anything while the section is merely
//! drawn: the voice list is read when its box is focused
//! ([`crate::widgets::voice_picker`]).

use leptos::prelude::*;
use lmgw_api_types::chat_voice::{turn_detection_label, TURN_DETECTIONS};
use lmgw_api_types::SettingsFull;

use super::{
    new_seed, source_label, Named, StageResolved, VoiceDraft, VoiceProblem, VoiceResolved,
};
use crate::widgets::voice_picker::{use_voices, VoiceInput};
use crate::widgets::{ModelPicker, Select};

/// What a stage resolves to, in a line: the alias, where it comes from,
/// where it runs, and what the GPU hold would answer with.
fn stage_line(s: &StageResolved) -> String {
    let Some(alias) = &s.alias else {
        return "in effect: none".to_string();
    };
    let mut line = format!("in effect: {alias} · {}", source_label(s.source.as_deref()));
    match s.local {
        // Off this machine: a provider, or a server elsewhere.
        Some(false) => line.push_str(" · remote"),
        Some(true) if s.cpu => line.push_str(" · local, CPU"),
        Some(true) => line.push_str(" · local"),
        None => {}
    }
    if let Some(f) = &s.fallback {
        line.push_str(&format!(
            " · under the GPU hold: {}{}",
            f.alias,
            if f.local { "" } else { " (remote)" }
        ));
    } else if let Some(u) = &s.fallback_unusable {
        line.push_str(&format!(
            " · under the GPU hold: refused (its fallback '{}' {})",
            u.alias, u.why
        ));
    }
    line
}

/// The voice in effect, in a line (§2.3): a name asked for, or realtime's
/// chain deciding from its default voice, or the model's own.
fn voice_line(v: &Named) -> String {
    match (&v.name, &v.inherits) {
        (Some(n), _) => format!("in effect: {n} · {}", source_label(v.source.as_deref())),
        (None, Some(d)) => format!(
            "in effect: realtime's default voice {d}, checked against the model when it speaks \
             · Settings → Realtime"
        ),
        (None, None) => "in effect: the model's default voice".to_string(),
    }
}

/// What an empty Voice box means, from the thread's resolution. A thread
/// that names its own voice is not resolved without it, so its box only
/// says "inherit".
fn voice_placeholder(r: Option<&VoiceResolved>) -> String {
    let Some(r) = r else {
        return "inherit".to_string();
    };
    match (&r.voice.name, r.voice.source.as_deref(), &r.voice.inherits) {
        (Some(n), Some("chat"), _) => format!("inherits {n}"),
        (Some(_), _, _) => "inherit".to_string(),
        (None, _, Some(d)) => format!("realtime's default {d}"),
        (None, _, None) => "the model's default".to_string(),
    }
}

/// A stage with no model anywhere, said once and quietly: most threads use
/// no voice, so it is no warning until a voice feature is pressed (WP7
/// says it there). `None` when every stage has a model.
fn not_set_up(problems: &[VoiceProblem]) -> Option<String> {
    let stages: Vec<&str> = problems
        .iter()
        .filter(|p| p.code == "not_configured")
        .map(|p| match p.stage.as_str() {
            "asr" => "speech-to-text",
            _ => "text-to-speech",
        })
        .collect();
    (!stages.is_empty()).then(|| {
        format!(
            "No {} model is set for voice: choose one above, in Settings → Chat → Voice or in \
             Settings → Realtime.",
            stages.join(" or ")
        )
    })
}

/// One "in effect" line under a field, when there is a thread to resolve.
fn effect(
    resolved: Option<Signal<Option<VoiceResolved>>>,
    line: impl Fn(&VoiceResolved) -> String + Send + Sync + 'static,
) -> impl IntoView {
    move || {
        resolved
            .and_then(|r| r.get())
            .map(|r| view! { <div class="field-hint">{line(&r)}</div> })
    }
}

/// The section. `resolved`: the thread's `voice_resolved` (a thread's
/// drawer); `None` for a folder's defaults, which also hide the seed.
#[component]
pub(in crate::pages) fn VoiceSection(
    draft: VoiceDraft,
    #[prop(optional)] resolved: Option<Signal<Option<VoiceResolved>>>,
) -> impl IntoView {
    let VoiceDraft {
        asr,
        tts,
        voice,
        style_mode,
        style,
        language,
        reply_language,
        read_aloud,
        turn,
        audio_input,
        seed,
    } = draft;
    let for_thread = resolved.is_some();
    // A folder has no resolution: the model a new thread would inherit is
    // the Chat's, then realtime's.
    let settings = (!for_thread)
        .then(|| LocalResource::new(|| crate::api::get::<SettingsFull>("/api/settings-full")));
    // The voice list of the model the thread would speak with: its own
    // choice, else what it inherits — not the saved resolution, which still
    // names an override the box no longer holds.
    let tts_now = Signal::derive(move || {
        let own = tts.get();
        if !own.trim().is_empty() {
            return own;
        }
        if let Some(r) = resolved {
            return r.get().and_then(|r| r.tts.inherited).unwrap_or_default();
        }
        match settings.and_then(|s| s.get()) {
            Some(Ok(s)) => [s.chat_tts_alias, s.realtime.tts_alias]
                .into_iter()
                .map(|a| a.trim().to_string())
                .find(|a| !a.is_empty())
                .unwrap_or_default(),
            _ => String::new(),
        }
    });
    let voices = use_voices(tts_now);
    let placeholder =
        Signal::derive(move || voice_placeholder(resolved.and_then(|r| r.get()).as_ref()));
    let read_opts = Signal::derive(|| {
        vec![
            (String::new(), "inherit".to_string()),
            ("on".to_string(), "on".to_string()),
            ("off".to_string(), "off".to_string()),
        ]
    });
    let turn_opts = Signal::derive(|| {
        std::iter::once((String::new(), "inherit".to_string()))
            .chain(
                TURN_DETECTIONS
                    .iter()
                    .map(|(n, l)| (n.to_string(), l.to_string())),
            )
            .collect::<Vec<_>>()
    });
    let style_opts = Signal::derive(|| {
        vec![
            (String::new(), "inherit".to_string()),
            ("none".to_string(), "none for this thread".to_string()),
            ("own".to_string(), "own".to_string()),
        ]
    });
    let audio_opts = Signal::derive(super::audio_input::options);
    // What blocks lmgw's own models now, on the Chat page (a folder's form
    // has no thread, and no verdict to change).
    let block = super::page::use_page_voice().map(|pv| pv.block);
    let lang_error = Memo::new(move |_| draft.error());
    view! {
        <div class="field voice-section" data-voice-section="">
            <label>"Voice"</label>
            <div class="field-grid" style="--field-min:160px">
                <div class="field">
                    <label>"Speech to text"</label>
                    <ModelPicker value=asr tasks=&["asr"] empty_label="inherit".to_string()/>
                    {effect(resolved, |r| stage_line(&r.asr))}
                </div>
                <div class="field">
                    <label>"Text to speech"</label>
                    <ModelPicker
                        value=tts
                        tasks=lmgw_api_types::realtime::SPEECH_TASKS
                        empty_label="inherit".to_string()
                    />
                    {effect(resolved, |r| stage_line(&r.tts))}
                </div>
            </div>
            <div class="field">
                <label>"Voice"</label>
                <VoiceInput
                    value=voice
                    on_input=Callback::new(move |v: String| voice.set(v))
                    list=voices
                    list_id=format!("voice-list-{}", if for_thread { "thread" } else { "folder" })
                    placeholder=placeholder
                />
                {effect(resolved, |r| voice_line(&r.voice))}
                {move || {
                    resolved
                        .and_then(|r| r.get())
                        .and_then(|r| r.voice.note)
                        .map(|n| view! { <div class="field-hint">{n}</div> })
                }}
            </div>
            <div class="field-grid" style="--field-min:160px">
                <div class="field">
                    <label>"Read aloud"</label>
                    <Select value=read_aloud options=read_opts/>
                    {effect(resolved, |r| format!(
                        "in effect: {} · {}",
                        if r.read_aloud.value { "on" } else { "off" },
                        source_label(r.read_aloud.source.as_deref())
                    ))}
                </div>
                <div class="field">
                    <label>"Turn detection"</label>
                    <Select value=turn options=turn_opts/>
                    {effect(resolved, |r| format!(
                        "in effect: {} · {}",
                        turn_detection_label(&r.turn_detection.value),
                        source_label(r.turn_detection.source.as_deref())
                    ))}
                </div>
            </div>
            <super::languages::LanguageFields
                language=language
                reply=reply_language
                resolved=resolved
            />
            {move || lang_error.get().map(|e| view! { <div class="notice warn">{e}</div> })}
            <div class="field" data-voice-audio-input="">
                <label title="experimental: the model that answers the turn (the thread's, or a fallback it is handed to, wherever it runs) hears it when it takes audio input (a speech-to-text model still transcribes each turn)">
                    "Audio input in voice mode"
                </label>
                <Select value=audio_input options=audio_opts/>
                {move || {
                    resolved.and_then(|r| r.get()).and_then(|r| r.audio_input).map(|a| {
                        // The live block, as the voice-mode chip reads it.
                        let a = block.and_then(|b| a.blocked(b.get())).unwrap_or(a);
                        view! {
                            <div class="field-hint">{a.effect_line()}</div>
                            <div
                                class="field-hint"
                                class:warn=a.block.is_some()
                                data-audio-path=a.path.clone()
                            >
                                {a.verdict_line()}
                            </div>
                        }
                    })
                }}
            </div>
            <div class="field">
                <label>"Speech style"</label>
                <Select value=style_mode options=style_opts/>
                <Show when=move || style_mode.get() == "own">
                    <textarea
                        class="input ta"
                        rows="2"
                        placeholder="calm, warm, unhurried"
                        prop:value=move || style.get()
                        on:input=move |ev| style.set(event_target_value(&ev))
                    ></textarea>
                </Show>
                {effect(resolved, |r| {
                    let text = if r.speech_style.text.is_empty() {
                        "none".to_string()
                    } else {
                        format!("\u{201c}{}\u{201d}", r.speech_style.text)
                    };
                    format!("in effect: {text} · {}", source_label(r.speech_style.source.as_deref()))
                })}
            </div>
            <Show when=move || for_thread>
                <div class="row voice-seed">
                    <span class="dim">
                        {move || {
                            // A drawn one waits for Save; else the thread's own.
                            let held = resolved.and_then(|r| r.get()).and_then(|r| r.seed);
                            match seed.get().or(held) {
                                Some(s) => format!("Voice seed {s}"),
                                None => "Voice seed: drawn on first use".to_string(),
                            }
                        }}
                    </span>
                    <button
                        type="button"
                        class="link-btn"
                        title="Draw another seed: a model that designs or draws its voice speaks with another one from the next reply (saved with Save)"
                        on:click=move |_| seed.set(Some(new_seed()))
                    >
                        "New voice"
                    </button>
                </div>
            </Show>
            {move || {
                resolved
                    .and_then(|r| r.get())
                    .map(|r| {
                        // A model that went away blocks what the thread is
                        // set up to do: a warning. Nothing set up at all is
                        // one quiet line.
                        let quiet = not_set_up(&r.problems)
                            .map(|t| view! { <div class="field-hint">{t}</div> });
                        // A clip without a transcript: a link to where it is
                        // transcribed (nothing is transcribed from here).
                        let loud = r
                            .problems
                            .into_iter()
                            .filter(|p| p.code != "not_configured")
                            .map(|p| {
                                let link = super::state::refusal_link(&p.code).map(|(href, label)| {
                                    view! { " " <a class="link-btn" href=href>{label}</a> }
                                });
                                view! { <div class="notice warn">{p.message}{link}</div> }
                            })
                            .collect_view();
                        view! { {loud} {quiet} }
                    })
            }}
            <div class="field-hint">
                "Empty fields inherit Settings → Chat → Voice, then Settings → Realtime."
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FallbackResolved, FallbackUnusable};
    use super::*;

    #[test]
    fn a_stage_line_names_source_place_and_hold_fallback() {
        let s = StageResolved {
            alias: Some("pocket".into()),
            source: Some("realtime".into()),
            local: Some(true),
            fallback: Some(FallbackResolved {
                alias: "openai/tts".into(),
                local: false,
            }),
            ..Default::default()
        };
        assert_eq!(
            stage_line(&s),
            "in effect: pocket · Settings → Realtime · local · under the GPU hold: openai/tts \
             (remote)"
        );
        let cpu = StageResolved {
            alias: Some("parakeet".into()),
            source: Some("thread".into()),
            local: Some(true),
            cpu: true,
            ..Default::default()
        };
        assert_eq!(
            stage_line(&cpu),
            "in effect: parakeet · this thread · local, CPU"
        );
        let unusable = StageResolved {
            alias: Some("asr".into()),
            source: Some("chat".into()),
            local: Some(true),
            fallback_unusable: Some(FallbackUnusable {
                alias: "audio/other".into(),
                why: "is itself a local model".into(),
            }),
            ..Default::default()
        };
        assert!(stage_line(&unusable).ends_with(
            "under the GPU hold: refused (its fallback 'audio/other' is itself a local model)"
        ));
        let remote = StageResolved {
            alias: Some("lan-tts".into()),
            source: Some("chat".into()),
            local: Some(false),
            ..Default::default()
        };
        assert_eq!(
            stage_line(&remote),
            "in effect: lan-tts · Settings → Chat · remote"
        );
        assert_eq!(stage_line(&StageResolved::default()), "in effect: none");
    }

    #[test]
    fn the_voice_line_and_placeholder_never_name_an_inherited_default_as_the_voice() {
        let named = Named {
            name: Some("alba".into()),
            source: Some("chat".into()),
            ..Default::default()
        };
        assert_eq!(voice_line(&named), "in effect: alba · Settings → Chat");
        let inherited = Named {
            source: Some("realtime".into()),
            inherits: Some("M5".into()),
            ..Default::default()
        };
        assert!(voice_line(&inherited).starts_with("in effect: realtime's default voice M5"));
        assert_eq!(
            voice_line(&Named::default()),
            "in effect: the model's default voice"
        );

        let with = |voice: Named| VoiceResolved {
            voice,
            ..Default::default()
        };
        assert_eq!(voice_placeholder(Some(&with(named))), "inherits alba");
        assert_eq!(
            voice_placeholder(Some(&with(inherited))),
            "realtime's default M5"
        );
        assert_eq!(
            voice_placeholder(Some(&with(Named::default()))),
            "the model's default"
        );
        let own = Named {
            name: Some("own".into()),
            source: Some("thread".into()),
            ..Default::default()
        };
        assert_eq!(voice_placeholder(Some(&with(own))), "inherit");
        assert_eq!(voice_placeholder(None), "inherit");
    }

    #[test]
    fn nothing_set_up_is_one_quiet_line() {
        let p = |stage: &str, code: &str| VoiceProblem {
            stage: stage.into(),
            code: code.into(),
            message: String::new(),
        };
        let both = not_set_up(&[p("asr", "not_configured"), p("tts", "not_configured")]).unwrap();
        assert!(
            both.starts_with("No speech-to-text or text-to-speech model is set for voice"),
            "{both}"
        );
        assert_eq!(not_set_up(&[p("tts", "unresolved")]), None);
        assert!(not_set_up(&[p("tts", "not_configured")])
            .unwrap()
            .starts_with("No text-to-speech model"));
    }
}
