//! A spoken result (design §8.4): the recording `audio__speak` stored,
//! played where the call was, with its model, voice and length, and Save
//! and Attach.

use leptos::prelude::*;
use lmgw_api_types::chat_threads::AttachmentMeta;

use super::{facts, secs, use_media};
use crate::chat_bytes::{attachment_path, ChatAudio, Saver};

#[component]
pub(super) fn AudioResult(a: AttachmentMeta) -> impl IntoView {
    let ctx = use_media();
    let saver = Saver::new();
    let f = facts(&a);
    let path = attachment_path(a.id);
    let mut line: Vec<String> = Vec::new();
    if let Some(m) = &f.model {
        line.push(m.clone());
    }
    if let Some(b) = &f.answered_by {
        line.push(format!("answered by {b}, fallback"));
    }
    if let Some(v) = &f.voice {
        line.push(format!("voice {v}"));
    }
    if let Some(ms) = f.duration_ms {
        line.push(secs(ms));
    }
    let (save_path, name) = (path.clone(), a.name.clone());
    let attach_a = a.clone();
    view! {
        <div class="media-audio" data-attachment=a.id.to_string()>
            // `none`: a thread with many recordings fetches none of them until one
            // is played (the facts line says the length).
            <ChatAudio path=path/>
            <div class="media-facts dim mono-sm">{line.join(" · ")}</div>
            <div class="media-actions">
                <button
                    class="btn sm"
                    title="Save the recording"
                    on:click=move |_| saver.save(save_path.clone(), name.clone())
                >
                    "Save"
                </button>
                <button
                    class="btn sm"
                    title="Add the recording to the next message, so the model gets it"
                    on:click=move |_| ctx.attach.run(attach_a.clone())
                >
                    "Attach"
                </button>
            </div>
        </div>
    }
}
