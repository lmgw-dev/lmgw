//! `/tokenize`, `/v1/audio/*`, `/v1/images/*` and `/v1/tasks/*` (api-docs
//! design §4.8, §5.3): llama.cpp's own tokenizer route, and the audio.cpp /
//! stable-diffusion.cpp byte-level passthroughs `proxy::audio` and
//! `proxy::image` forward with only alias resolution and model rewrite —
//! there is no IR translation for any of these, so the request shapes here
//! are the upstream's own, as lmgw reads just enough of them to route.

use schemars::generate::SchemaGenerator;
use schemars::Schema;
use serde_json::{json, Value};

use super::super::schemas;

// ---------------------------------------------------------------------------
// POST /tokenize (§5.3)
// ---------------------------------------------------------------------------

pub(crate) fn tokenize_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "TokenizeRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model"],
            "properties": {
                "model": {"type": "string", "description": "Required: llama-server's router \
                    mode has no other way to pick a backend. A missing model is a 400 naming \
                    GET /v1/models."},
                "content": {"description": "A string, or a mixed array of strings and token \
                    ids."},
                "add_special": {"type": "boolean", "default": false},
                "parse_special": {"type": "boolean", "default": true},
                "with_pieces": {"type": "boolean", "default": false}
            },
            "additionalProperties": true,
            "description": "Every key but model is forwarded to the backend's own /tokenize \
                verbatim. Refused with 501 not_supported_error on any backend but a real \
                llama-server — an id from a tokenizer the backend does not run would be a \
                hidden approximation (§10 choice 6)."
        }),
    )
}

pub(crate) fn tokenize_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "TokenizeResponse",
        schemars::json_schema!({
            "type": "object",
            "required": ["tokens"],
            "properties": {
                "tokens": {
                    "description": "An array of integer token ids, or — with_pieces — an \
                        array of {id, piece} (piece a string, or a byte array when it is not \
                        valid UTF-8)."
                }
            }
        }),
    )
}

pub(crate) fn tokenize_example() -> Value {
    json!({"model": "<pick a model>", "content": "Hello, world!"})
}

// ---------------------------------------------------------------------------
// /v1/audio/* (proxy::audio — byte-level passthrough, OpenAI-protocol only)
// ---------------------------------------------------------------------------

/// Content types the audio routes may answer with — audio.cpp's own choice,
/// driven by `response_format`/the upstream's default.
pub(crate) const AUDIO_MIME_TYPES: &[&str] = &[
    "audio/mpeg",
    "audio/wav",
    "audio/ogg",
    "audio/flac",
    "audio/aac",
    "audio/opus",
    "audio/pcm",
];

pub(crate) fn speech_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AudioSpeechRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "input"],
            "properties": {
                "model": {"type": "string"},
                "input": {"type": "string"},
                "voice": {"type": "string", "description": "A row's own voice preset id, \
                    when it defines any (GET /v1/audio/voices). A voice-library clip with no \
                    transcript, for a model that cannot clone one without, is 400 \
                    voice_needs_transcript before anything starts (lmgw.needs_transcript in \
                    the voice list); send reference_text, or transcribe the clip in the Audio \
                    lab. A local model whose audio.cpp image lacks eSpeak NG is 400 \
                    audio_image_lacks_espeak."},
                "response_format": {"type": "string"},
                "speed": {"type": "number"},
                "instructions": {"type": "string", "description": "A speaking style, or the \
                    description a voice-design model builds its voice from (required there: \
                    400 instructions_required without one, unless the row has a default). \
                    Dropped for a model that reads none — capabilities.speech.instructions \
                    says which, x-lmgw-speech says when."},
                "language": {"type": "string", "description": "An ISO 639-1 code or BCP-47 \
                    tag; a local model gets it in its own vocabulary."},
                "stream_format": {"type": "string", "enum": ["sse", "audio"],
                    "description": "Stream PCM16 (response_format pcm): speech.audio.delta events, or raw \
                    chunked PCM. A local model streams only when its row runs in streaming \
                    mode — an offline row is 400 streaming_unsupported before anything starts \
                    (capabilities.speech.streaming says which); x-lmgw-sample-rate states the \
                    PCM's rate."}
            },
            "additionalProperties": true,
            "description": "OpenAI's TTS shape plus audio.cpp's own request options, \
                forwarded with model rewritten to the upstream's own id; a row's configured \
                voice presets apply on top. input may carry inline tags ([laughs], or a stage \
                direction such as (sighs)): rendered by a model whose \
                capabilities.speech.inline_tags says so, stripped everywhere else — never read \
                out. An input of nothing but tags is 400 empty_input, refused before any \
                model is started."
        }),
    )
}

pub(crate) fn transcriptions_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AudioTranscriptionsRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["file", "model"],
            "properties": {
                "file": {"type": "string", "format": "binary"},
                "model": {"type": "string"},
                "language": {"type": "string"},
                "prompt": {"type": "string"},
                "response_format": {"type": "string"},
                "temperature": {"type": "number"}
            },
            "description": "multipart/form-data (also accepted as application/json with the \
                same fields, file base64-less — the multipart form is what real clients \
                send)."
        }),
    )
}

pub(crate) fn transcription_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AudioTranscription",
        schemars::json_schema!({
            "type": "object",
            "required": ["text"],
            "properties": {"text": {"type": "string"}},
            "additionalProperties": true
        }),
    )
}

pub(crate) fn transcription_details_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AudioTranscriptionDetails",
        schemars::json_schema!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "text": {"type": "string"},
                "words": {"type": "array", "items": {"type": "object", "additionalProperties": true}},
                "segments": {"type": "array", "items": {"type": "object", "additionalProperties": true}},
                "speaker_turns": {"type": "array", "items": {"type": "object", "additionalProperties": true}}
            },
            "additionalProperties": true,
            "description": "The same transcription, plus the detail arrays the plain route \
                leaves out."
        }),
    )
}

pub(crate) fn alignments_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AudioAlignmentsRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["file", "model", "text"],
            "properties": {
                "file": {"type": "string", "format": "binary"},
                "model": {"type": "string"},
                "text": {"type": "string"},
                "language": {"type": "string"}
            },
            "description": "multipart/form-data only — lmgw's own route (task: \"align\" \
                rows), not an OpenAI shape."
        }),
    )
}

pub(crate) fn alignments_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AudioAlignments",
        schemars::json_schema!({"type": "object", "additionalProperties": true}),
    )
}

pub(crate) fn voices_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AudioVoices",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "voices": {"type": "array", "items": {"type": "string"},
                    "description": "Every name the model answers to as `voice`, sorted."},
                "lmgw": {
                    "type": "object",
                    "description": "Only on a local audio model answered from lmgw's catalog \
                        (x-lmgw-voices-source: config).",
                    "properties": {
                        "source": {"type": "string", "enum": ["config"]},
                        "engine_asked": {"type": "boolean"},
                        "held": {"type": "boolean", "description": "The GPU hold (or a \
                            benchmark run) holds the card and the model has no fallback: the \
                            list stands, its speech is refused (503 gpu_hold) until the hold \
                            is released."},
                        "entries": {"type": "array", "items": {
                            "type": "object",
                            "required": ["id", "kind", "send"],
                            "properties": {
                                "id": {"type": "string"},
                                "kind": {"type": "string",
                                    "enum": ["preset", "library", "native", "embedding"]},
                                "send": {"type": "string", "enum": ["voice", "options.voice_id"],
                                    "description": "Where lmgw puts the name in the engine's \
                                        request; clients always send it as `voice`."},
                                "transcript": {"type": "boolean",
                                    "description": "A library clip's transcript is recorded."}
                            }
                        }},
                        "default": {"type": ["string", "null"],
                            "description": "What speaks when a request names no voice."},
                        "missing": {"type": "array", "items": {"type": "string"},
                            "description": "Voices the spec names whose file the package \
                                lacks."},
                        "needs_transcript": {"type": "boolean",
                            "description": "The model cannot clone a library clip without its \
                                transcript: speech with an entry whose `transcript` is false \
                                is refused (400 voice_needs_transcript) until it has one \
                                (Audio lab → Transcribe)."}
                    }
                }
            },
            "additionalProperties": true,
            "description": "lmgw's own route. A local audio model is answered from lmgw's \
                catalog without starting it (?probe=engine asks its own server, starting it); \
                a remote one is asked."
        }),
    )
}

// ---------------------------------------------------------------------------
// /v1/images/* (proxy::image)
// ---------------------------------------------------------------------------

pub(crate) fn image_generations_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ImageGenerationsRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "prompt"],
            "properties": {
                "model": {"type": "string"},
                "prompt": {"type": "string", "description": "May carry a trailing \
                    <sd_cpp_extra_args>{…}</sd_cpp_extra_args> block of raw sd.cpp CLI \
                    arguments — the api-types image_lab.rs builder is the reference for its \
                    contents."},
                "n": {"type": "integer"},
                "size": {"type": "string"},
                "output_format": {"type": "string"},
                "output_compression": {"type": "integer"}
            },
            "additionalProperties": true,
            "description": "Other keys are forwarded for a cloud image upstream."
        }),
    )
}

pub(crate) fn image_response(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ImageGenerationsResponse",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "created": {"type": "integer"},
                "data": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"b64_json": {"type": "string"}}
                    }
                }
            }
        }),
    )
}

pub(crate) fn image_generations_example() -> Value {
    json!({"model": "<pick a model>", "prompt": "A red bicycle in a park.", "n": 1})
}

pub(crate) fn image_edits_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "ImageEditsRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["image", "model", "prompt"],
            "properties": {
                "image": {
                    "type": "array",
                    "items": {"type": "string", "format": "binary"},
                    "description": "One or more image files — proxy/image.rs relays every \
                        multipart field named image verbatim, with no cardinality check of \
                        its own."
                },
                "mask": {"type": "string", "format": "binary"},
                "model": {"type": "string"},
                "prompt": {"type": "string"},
                "n": {"type": "integer"},
                "size": {"type": "string"}
            },
            "description": "Refused unless the resolved row's edit pipeline flag is set."
        }),
    )
}

// ---------------------------------------------------------------------------
// /v1/tasks/* (audio.cpp's own generic task route)
// ---------------------------------------------------------------------------

pub(crate) fn task_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "TaskRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["model", "request"],
            "properties": {
                "model": {"type": "string"},
                "request": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Relayed to audio.cpp's generic task endpoint untouched — \
                        its shape is the task's own, not modeled here."
                }
            }
        }),
    )
}
