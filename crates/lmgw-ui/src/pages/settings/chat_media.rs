//! Settings → Chat → Picture and sound tools (chat-media-tools design
//! §3.8): the model each image and audio tool uses when the chat model names
//! none, and the voice that goes with the default speech model. A folder and
//! a chat can pick their own; `/mcp`, `/v1/responses` and agent runs use
//! these.
//!
//! The pickers list what each tool's `model` enum lists
//! (`GET /chat/api/media/defaults`, read again after every save); a saved
//! name that no longer resolves stays picked, marked unavailable with the
//! gateway's reason ([`lmgw_chat_ui::media_defaults`]).

use leptos::prelude::*;
use lmgw_api_types::chat_media::{MediaDefaultsView, MediaField};

use super::{bridge, f, Ctl, Def, Page};
use crate::widgets::Field;
use lmgw_chat_ui::media_defaults::{use_view, MediaModelBox, MediaVoiceBox};

/// The group's rows, in page order.
pub(super) const CHAT_MEDIA: &[Def] = &[
    f(
        "chat",
        "chat-media",
        "chat_media_defaults.generate",
        "Drawing pictures",
        Ctl::MediaModel(MediaField::Generate),
    )
    .hint("the image model a new picture is drawn with when the chat model names none")
    .terms("chat media image picture draw generate default model tool image__generate"),
    f(
        "chat",
        "chat-media",
        "chat_media_defaults.edit",
        "Editing pictures",
        Ctl::MediaModel(MediaField::Edit),
    )
    .hint("the image edit model a picture is changed with when the chat model names none")
    .terms("chat media image picture edit variation default model tool image__edit"),
    f(
        "chat",
        "chat-media",
        "chat_media_defaults.speak",
        "Speaking text",
        Ctl::MediaModel(MediaField::Speak),
    )
    .hint("the text-to-speech model a spoken recording is made with when the chat model names none")
    .terms("chat media audio speak tts speech default model tool audio__speak"),
    f(
        "chat",
        "chat-media",
        "chat_media_defaults.voice",
        "Voice",
        Ctl::MediaVoice,
    )
    .hint("the voice the speech model above speaks with when the chat model names none")
    .terms("chat media audio speak voice default tts"),
    f(
        "chat",
        "chat-media",
        "chat_media_defaults.transcribe",
        "Transcribing recordings",
        Ctl::MediaModel(MediaField::Transcribe),
    )
    .hint("the speech-to-text model a recording is transcribed with when the chat model names none")
    .terms("chat media audio transcribe asr speech to text default model tool audio__transcribe"),
];

/// The gateway's view of the defaults, shared by the group's fields.
#[derive(Clone, Copy)]
pub(super) struct MediaView(Memo<Option<MediaDefaultsView>>);

/// Read the view for the page, again whenever the saved settings change.
pub(super) fn provide(page_data: RwSignal<Option<serde_json::Value>>) {
    let again = Signal::derive(move || {
        page_data.with(|d| {
            d.as_ref()
                .map(|v| v["chat_media_defaults"].to_string())
                .unwrap_or_default()
        })
    });
    provide_context(MediaView(use_view(Signal::stored(String::new()), again)));
}

/// One field of the group.
#[component]
pub(super) fn MediaDefaultField(
    d: &'static Def,
    page: Page,
    field: MediaField,
    dirty: Signal<bool>,
    error: Signal<Option<String>>,
    id: String,
    hidden: Signal<bool>,
) -> impl IntoView {
    let form = page.form;
    let value = bridge(form, d.key);
    let Some(MediaView(view)) = use_context::<MediaView>() else {
        return ().into_any();
    };
    let control = if field == MediaField::Voice {
        let speak = Signal::derive(move || form.text("chat_media_defaults.speak"));
        view! { <MediaVoiceBox value=value speak=speak view=view inherit=false bare=true/> }
            .into_any()
    } else {
        view! { <MediaModelBox value=value field=field view=view inherit=false bare=true/> }
            .into_any()
    };
    view! {
        <Field label=d.label unit=d.unit hint=d.hint dirty=dirty error=error id=id hidden=hidden>
            {control}
        </Field>
    }
    .into_any()
}
