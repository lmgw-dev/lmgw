//! A model's `task` and the routes it drives (model-capabilities design
//! §2.1, §4, §7).
//!
//! **A cloud model's task** is what its catalog states. Where the catalog
//! states nothing — OpenAI's own `/v1/models` lists `{id, object, created,
//! owned_by}` and no modalities at all — it is what the name says by
//! OpenAI's naming ([`by_name`]), and only then `chat`. Taking every silent
//! entry for a chat model was realtime live run 3's D1: `gpt-4o-mini-tts`
//! and `gpt-4o-mini-transcribe` advertised the chat routes and
//! `/v1/realtime`, and a realtime session, the realtime settings' save
//! check and the Chat's transcription setting all refused them as the
//! speech models they are. The owner's `capabilities_override` task wins
//! over both (§7), and its routes follow it ([`endpoints`]).
//!
//! **The routes a task answers on** ([`endpoints`]) are one table, for a
//! cloud entry and for an override that changes a task: the chat routes by
//! protocol, the embedding, rerank and image routes, the speech and
//! transcription routes on the OpenAI protocol only (lmgw's `/v1/audio/*`
//! refuses every other), `/v1/systemone` for a decision model on the OpenAI
//! protocol only, none for an OpenAI Realtime model, and audio.cpp's
//! generic task routes for its other tasks. A local audio row publishes its
//! own, richer list (`for_audio_with`: the voice list, the detail route).

use super::{
    strings, CHAT_ENDPOINTS_OPENAI, CHAT_ENDPOINTS_OTHER, IMAGE_EDITS_ENDPOINT,
    IMAGE_GENERATIONS_ENDPOINT,
};
use crate::config::Protocol;

/// The task an OpenAI Realtime model publishes: it speaks only over the
/// provider's own Realtime WebSocket, which lmgw does not relay — no lmgw
/// route serves it, and it is never a chat model. (lmgw's `/v1/realtime`
/// answers the bare name with `realtime.default_model`, realtime design
/// §5.1.)
pub const REALTIME_TASK: &str = "realtime";

/// The task OpenAI's naming gives `id` — the name of a model whose catalog
/// says nothing about it (module doc) — or `None` for a chat model:
/// - an OpenAI Realtime name (`gpt-realtime*`, `gpt-4o*-realtime*`):
///   [`REALTIME_TASK`];
/// - `tts` as a word of the name (`tts-1`, `tts-1-hd`, `gpt-4o-mini-tts`,
///   `gpt-4o-mini-tts-2025-03-20`): `tts`;
/// - `transcribe` in it (`gpt-4o-transcribe`, `gpt-4o-mini-transcribe`), or
///   a word starting `whisper` (`whisper-1`, `whisper-large-v3`,
///   `distil-whisper-large-v3-en`): `asr`.
///
/// A vendor prefix (`openai/gpt-4o-mini-tts`) is no part of the name.
pub fn by_name(id: &str) -> Option<&'static str> {
    let name = id.rsplit('/').next().unwrap_or(id).to_ascii_lowercase();
    if crate::realtime::resolve::is_openai_realtime_name(&name) {
        return Some(REALTIME_TASK);
    }
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.contains(&"tts") {
        return Some("tts");
    }
    if name.contains("transcribe") || words.iter().any(|w| w.starts_with("whisper")) {
        return Some("asr");
    }
    None
}

/// The lmgw routes a model of `task` answers on, served by an upstream of
/// `protocol` (module doc).
pub fn endpoints(task: &str, protocol: Protocol) -> Vec<String> {
    // llama-server answers OpenAI's chat routes and `/v1/audio/transcriptions`.
    let openai = protocol.speaks_openai_http();
    match task {
        "chat" if openai => strings(&CHAT_ENDPOINTS_OPENAI),
        "chat" => strings(&CHAT_ENDPOINTS_OTHER),
        "embedding" => strings(&["/v1/embeddings"]),
        "rerank" => strings(&["/v1/rerank"]),
        // `/v1/systemone` forwards to `openai` and `llama_cpp` upstreams only
        // (decision-models design §2.9, §4.1).
        "decision" if openai => strings(&["/v1/systemone"]),
        "decision" => Vec::new(),
        "image_generation" | "video_generation" => strings(&[IMAGE_GENERATIONS_ENDPOINT]),
        "image_edit" => strings(&[IMAGE_GENERATIONS_ENDPOINT, IMAGE_EDITS_ENDPOINT]),
        // `/v1/audio/*` requires an OpenAI-protocol upstream.
        "tts" | "vdes" if openai => strings(&["/v1/audio/speech"]),
        "asr" if openai => strings(&["/v1/audio/transcriptions"]),
        "align" if openai => strings(&["/v1/audio/alignments"]),
        "tts" | "vdes" | "asr" | "align" => Vec::new(),
        REALTIME_TASK => Vec::new(),
        _ => strings(&["/v1/tasks/run", "/v1/tasks/stream"]),
    }
}

/// Whether `task` is a cloud catalog's image or video model, which publishes
/// no route ([`unverified_media_route_note`]).
pub fn is_unverified_cloud_media(task: &str) -> bool {
    matches!(task, "image_generation" | "image_edit" | "video_generation")
}

/// Why a cloud image or video model publishes no route (decision-models
/// design §6.2): lmgw's image routes post to the upstream's
/// `/images/generations` and `/images/edits`, and no provider that states an
/// image or video output in its catalog is verified to serve those. OpenRouter,
/// whose catalog shape this is, documents `POST /images` and an asynchronous
/// `POST /videos` instead.
pub fn unverified_media_route_note(upstream_name: &str, task: &str) -> String {
    let what = if task == "video_generation" {
        "video"
    } else {
        "images"
    };
    format!(
        "The catalog of upstream {upstream_name} says this model outputs {what} and no text. \
         No lmgw route is verified to reach it there: /v1/images/generations and \
         /v1/images/edits post to the upstream's /images/generations and /images/edits, and \
         OpenRouter documents POST /images for images and an asynchronous POST /videos for \
         video instead. So no endpoint is published. A capabilities_override on an alias can \
         state the endpoints of a provider that serves them."
    )
}

/// The note of a cloud entry whose task its name gave ([`by_name`]).
pub fn by_name_note(upstream_name: &str, task: &str) -> String {
    let what = match task {
        REALTIME_TASK => "an OpenAI Realtime model, which speaks only over the provider's own \
                          Realtime WebSocket — lmgw does not relay it, so no route here serves \
                          it (lmgw's /v1/realtime answers the bare name with \
                          realtime.default_model)"
            .to_string(),
        "tts" => "a text-to-speech model, served on /v1/audio/speech".to_string(),
        "asr" => "a speech-to-text model, served on /v1/audio/transcriptions".to_string(),
        other => format!("a '{other}' model"),
    };
    format!(
        "The catalog of upstream {upstream_name} states no task for this model; by OpenAI's \
         naming it is {what}. A capabilities_override task on an alias of it says otherwise."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_names_say_what_they_are() {
        for (id, want) in [
            ("gpt-4o-mini-tts", Some("tts")),
            ("openai/gpt-4o-mini-tts", Some("tts")),
            ("gpt-4o-mini-tts-2025-03-20", Some("tts")),
            ("tts-1", Some("tts")),
            ("tts-1-hd", Some("tts")),
            ("TTS-1-HD", Some("tts")),
            ("gpt-4o-transcribe", Some("asr")),
            ("gpt-4o-mini-transcribe", Some("asr")),
            ("gpt-4o-transcribe-diarize", Some("asr")),
            ("whisper-1", Some("asr")),
            ("whisper-large-v3-turbo", Some("asr")),
            ("distil-whisper-large-v3-en", Some("asr")),
            ("gpt-realtime", Some(REALTIME_TASK)),
            ("gpt-realtime-mini", Some(REALTIME_TASK)),
            ("gpt-4o-realtime-preview", Some(REALTIME_TASK)),
            ("openai/gpt-4o-mini-realtime-preview", Some(REALTIME_TASK)),
            // Chat models, and words that only contain the letters.
            ("gpt-4o-mini", None),
            ("gpt-4o-audio-preview", None),
            ("gpt-5.1", None),
            ("o4-mini", None),
            ("mattson-7b", None),
            ("text-embedding-3-small", None),
        ] {
            assert_eq!(by_name(id), want, "{id}");
        }
    }

    #[test]
    fn a_task_s_routes_follow_the_protocol() {
        assert_eq!(endpoints("tts", Protocol::Openai), ["/v1/audio/speech"]);
        assert_eq!(
            endpoints("asr", Protocol::Openai),
            ["/v1/audio/transcriptions"]
        );
        assert!(endpoints("tts", Protocol::Gemini).is_empty());
        assert!(endpoints(REALTIME_TASK, Protocol::Openai).is_empty());
        assert_eq!(endpoints("chat", Protocol::Openai).len(), 4);
        assert_eq!(endpoints("chat", Protocol::Anthropic).len(), 3);
        assert_eq!(
            endpoints("sep", Protocol::Openai),
            ["/v1/tasks/run", "/v1/tasks/stream"]
        );
    }
}
