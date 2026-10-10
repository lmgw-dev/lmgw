//! `/audio-lab/api/*`, the Audio lab: the model list its forms are built
//! from, the voice library of reference clips (list, upload, preview, delete,
//! transcripts), and the four routes that hand a request to the matching
//! `/v1` handler. The shapes are `lmgw-api-types`' `audio_lab` module, the
//! types the handlers read and write; the four hand-offs reuse the `/v1`
//! siblings' request and answer schemas. Admin routes (`Cap::Admin`), so
//! they appear in the admin document only.

use lmgw_api_types::audio_lab as lab;
use lmgw_api_types::openapi_ext::model_task;
use schemars::generate::SchemaGenerator;
use schemars::Schema;

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use super::super::schemas;
use super::super::v1::media;

fn route(
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    description: &'static str,
) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "audio-lab",
        summary,
        description,
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::NoContent,
        dialect: Dialect::Lab,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        example: None,
    }
}

/// A hand-off to a `/v1` handler: every failure of it, a body the lab cannot
/// read included, is the OpenAI envelope the `/v1` route answers in, not the
/// lab's `{error}`.
macro_rules! handoff {
    ($method:expr, $path:expr, $summary:expr, $description:expr $(,)?) => {
        DocRoute {
            dialect: Dialect::OpenAi,
            ..route(
                $method,
                $path,
                $summary,
                concat!(
                    $description,
                    " Every failure is the `/v1` route's OpenAI envelope, relayed as it came."
                ),
            )
        }
    };
}

const CLIP_MIME_TYPES: &[&str] = &[
    "audio/wav",
    "audio/mpeg",
    "audio/ogg",
    "audio/flac",
    "audio/aac",
    "application/octet-stream",
];

fn voices_answer(g: &mut SchemaGenerator) -> Schema {
    let listed = media::voices_response(g).to_value();
    let idle = g.subschema_for::<lab::VoicesNotRunning>().to_value();
    schemars::json_schema!({
        "description": "The model's voice list, as GET /v1/audio/voices answers it; or, for \
            a local model that is not running (and `start` not sent), {voices: [], running: \
            false}.",
        "anyOf": [listed, idle]
    })
}

fn upload_request(g: &mut SchemaGenerator) -> Schema {
    schemas::named(
        g,
        "AudioLabUploadRequest",
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "file": {
                    "type": "string",
                    "format": "binary",
                    "description": "A clip; send as many file parts as there are clips. A \
                        part's file name names the clip: letters, digits, space, '.', '_' \
                        and '-', not starting with '.' (anything else is a 400). A clip \
                        with the name of a stored one replaces it."
                },
                "transcript": {
                    "type": "string",
                    "description": "What the clip says. Applied only when the upload holds \
                        exactly one file; blank is ignored."
                }
            },
            "required": ["file"],
            "description": "multipart/form-data. The body size is not limited."
        }),
    )
}

fn transcription_answer(g: &mut SchemaGenerator) -> Schema {
    let plain = media::transcription_response(g).to_value();
    let detailed = media::transcription_details_response(g).to_value();
    schemars::json_schema!({
        "description": "The transcription as /v1/audio/transcriptions answers it; with \
            ?details=1 as /v1/audio/transcriptions/details answers it.",
        "anyOf": [plain, detailed]
    })
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::AudioLabModels>()),
            ..route(
                "GET",
                "/audio-lab/api/models",
                "List the audio models",
                "The enabled audio models with what a form needs to shape itself: `task` \
                 decides text-to-speech, speech-to-text or another audio.cpp task, `mode` \
                 whether streaming is offered. GET /v1/models flattens every upstream to a \
                 few fields and cannot say this. The audio containers' state rides along so a \
                 client can say \"not running\" before a request fails, as does where the \
                 voice library lives (`voices_dir` on the host, `container_voices_dir` in \
                 the container) and whether the audio class's voice directory is that library.",
            )
        },
        DocRoute {
            query: Some(|g| g.root_schema_for::<lab::LabVoicesQuery>()),
            response: Resp::Json(voices_answer),
            dialect: Dialect::OpenAi,
            ..route(
                "GET",
                "/audio-lab/api/voices",
                "List a model's voices",
                "The built-in voice ids and configured presets of a text-to-speech model, \
                 for a voice picker. Unlike GET /v1/audio/voices, which answers a local model \
                 from the gateway's own catalog, this asks the model's own server, and only \
                 while it is already running: opening a page must not load a model onto the \
                 GPU. A local model that is not running answers {voices: [], running: false}; \
                 `start=1` starts it and asks (what /v1/audio/voices does with \
                 probe=engine). An empty list is normal: the model carries no cached voice \
                 ids and wants a reference clip. Failures are /v1/audio/voices' own OpenAI \
                 envelope.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::ClipList>()),
            ..route(
                "GET",
                "/audio-lab/api/refs",
                "List the voice library",
                "The reference clips stored in the voice library (the `voices` folder of \
                 the audio models directory), by file name, each with the path the model \
                 container sees (`server_path`: what a request's `voice_ref` takes), the \
                 `voice` name it answers to once the audio class's voice directory is this \
                 library, and its transcript. Without an audio models directory the answer \
                 is a 200 with no clips and an `error` saying so.",
            )
        },
        DocRoute {
            request: Req::Multipart(upload_request),
            response: Resp::Json(|g| g.root_schema_for::<lab::ClipsUploaded>()),
            ..route(
                "POST",
                "/audio-lab/api/refs",
                "Upload reference clips",
                concat!(
                    "Stores one or more clips in the voice library, each written whole or not \
                     at all. audio.cpp takes a clone's reference as a path on the server \
                     rather than an upload, which is what the library is for: a stored clip \
                     reads in the container as /models/voices/<name>. A `transcript` field \
                     with exactly one file is written to the library's transcript index. \
                     Otherwise, when the audio settings name a transcription model, each \
                     clip is transcribed with it: `transcribed` has an entry per clip, \
                     either who wrote the transcript or why none was written; the upload \
                     stands either way. 400 for a bad file name, a malformed body, no file \
                     field or an audio models directory that is not configured; 500 when the disk refuses. ",
                    "A gateway started from a development build whose audio models directory \
                     is the installed app's refuses the write (400, code \
                     dev_shared_models_dir). The body size is not limited."
                ),
            )
        },
        DocRoute {
            response: Resp::Binary(CLIP_MIME_TYPES),
            ..route(
                "GET",
                "/audio-lab/api/refs/{name}",
                "Read a reference clip",
                "The stored clip's bytes, with the content type its extension implies, for \
                 a player. 400 \"invalid clip\" for a name that is not a plain file name, 404 \
                 for a clip that is not there; both answer `{error}`.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::ClipsAck>()),
            ..route(
                "POST",
                "/audio-lab/api/refs/{name}/delete",
                "Delete a reference clip",
                concat!(
                    "Deletes the clip and its transcript line; a transcript left behind \
                     would attach itself to the next clip uploaded under the name. Answers \
                     the library after the change. 400 \"invalid clip\" for a name that is \
                     not a plain file name; 500 when the file cannot be removed (a clip \
                     that is not there included). ",
                    "A development gateway whose audio models directory is the installed \
                     app's refuses (400, code dev_shared_models_dir)."
                ),
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<lab::SetRefText>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::ClipsAck>()),
            ..route(
                "POST",
                "/audio-lab/api/refs/{name}/text",
                "Set a clip's transcript",
                concat!(
                    "Sets, or with an empty string drops, the clip's line in the library's \
                     transcript index. This is what makes a library clip a voice and not \
                     just a path: when the audio class's voice directory is the library, a \
                     request with \"voice\": \"<name>\" clones the clip and the engine reads \
                     this line as the reference text the caller did not send. A cloning \
                     model without one reads the target text in an unconditioned voice. \
                     404 for a clip that is not there; 400 \"invalid clip\" for a bad name; 422 \
                     when the body is not JSON of the shape `{transcript: string}` (answered \
                     `{error}` with the reason). ",
                    "A development gateway whose audio models directory is the installed \
                     app's refuses (400, code dev_shared_models_dir)."
                ),
            )
        },
        DocRoute {
            request: Req::OptionalJson(|g| g.root_schema_for::<lab::TranscribeRef>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::ClipTranscribed>()),
            ..route(
                "POST",
                "/audio-lab/api/refs/{name}/transcribe",
                "Transcribe a reference clip",
                "Has a speech-to-text model write the clip's transcript, replacing one it \
                 had. The body is optional; one whose `alias` is not a string is a 422 \
                 (answered `{error}` with the reason), not a silent fall back to another \
                 model. `name` is the file name or the voice name. The model is the body's \
                 `alias`, else the audio settings' clip-transcription model; with neither \
                 the answer is a 400 saying so. The model may be local or a cloud alias, and \
                 a configured fallback answers as for any request when the GPU hold or an \
                 outside process keeps the model off the card: `transcript_source`, \
                 `answered_by`, `fallback_reason` and `message` say who wrote it. 400 also \
                 when the alias is no speech-to-text model, the clip is unreadable, the model \
                 heard no speech, or it failed. Unlike the dashboard's bulk operation \
                 (voice_transcribe) this is one clip.",
            )
        },
        DocRoute {
            request: Req::Json(media::speech_request),
            response: Resp::Binary(media::AUDIO_MIME_TYPES),
            model_task: Some(model_task::TTS),
            writes: Some(false),
            ..handoff!(
                "POST",
                "/audio-lab/api/speech",
                "Synthesize speech",
                "POST /v1/audio/speech, dispatched in the gateway: the same handler, so \
                 alias resolution, the model rewrite, error normalization and logging are \
                 the route's own and the call appears in the request log. Only the gateway's \
                 key check and the HTTP hop are skipped; the call is the dashboard's \
                 and is not budgeted or scoped to a key. The request and the answer are \
                 the /v1 route's, relayed verbatim: a WAV or other audio body, or with \
                 stream_format an event stream or chunked PCM, and an error in the \
                 OpenAI envelope. The body size is not limited.",
            )
        },
        DocRoute {
            query: Some(|g| g.root_schema_for::<lab::DetailsQuery>()),
            request: Req::Multipart(media::transcriptions_request),
            response: Resp::Json(transcription_answer),
            model_task: Some(model_task::ASR),
            writes: Some(false),
            ..handoff!(
                "POST",
                "/audio-lab/api/transcriptions",
                "Transcribe audio",
                "POST /v1/audio/transcriptions, dispatched in the gateway (see \
                 /audio-lab/api/speech for what that means), with both of that route's \
                 request shapes: a multipart upload, or JSON naming a clip on the model \
                 server by path. `details=1` sends the request to \
                 /v1/audio/transcriptions/details instead, which also answers with the word \
                 timings, segments and speaker turns the model produced; the lab is where \
                 to find out whether a model produces any. The body size is not limited.",
            )
        },
        DocRoute {
            request: Req::Multipart(media::alignments_request),
            response: Resp::Untyped("audio.cpp's own alignment JSON, relayed verbatim"),
            model_task: Some(model_task::ASR),
            writes: Some(false),
            ..handoff!(
                "POST",
                "/audio-lab/api/alignments",
                "Align audio to text",
                "POST /v1/audio/alignments, dispatched in the gateway (see \
                 /audio-lab/api/speech): forced alignment for a model whose task is align, \
                 multipart upload only. The body size is not limited.",
            )
        },
        DocRoute {
            query: Some(|g| g.root_schema_for::<lab::TaskRunQuery>()),
            request: Req::Json(media::task_request),
            response: Resp::Untyped("audio.cpp's own per-task JSON, relayed verbatim"),
            writes: Some(false),
            ..handoff!(
                "POST",
                "/audio-lab/api/tasks/run",
                "Run a generic task",
                "POST /v1/tasks/run, dispatched in the gateway (see /audio-lab/api/speech): \
                 audio.cpp's generic task route, the way to reach the tasks with no OpenAI \
                 shape. `stream=1` sends it to /v1/tasks/stream instead, which a \
                 streaming-mode model answers with its buffered event list. The answer is \
                 the task's own JSON, relayed. The body size is not limited.",
            )
        },
    ]
}
