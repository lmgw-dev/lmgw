//! `/voice-lab/api/*`, the Voices lab: what a text-to-speech alias takes,
//! the sentence per language, the rounds of candidates, the samples saved
//! to the voice library with how each was made, and the cloning check. The
//! shapes are `lmgw-api-types`' `voice_lab` module, the types the handlers
//! read and write. Admin routes (`Cap::Admin`), so they appear in the admin
//! document only. The round and the save are also the `voice_generate` and
//! `voice_sample_save` ops.

use lmgw_api_types::openapi_ext::model_task;
use lmgw_api_types::voice_lab as lab;

use super::super::registry::{Dialect, DocRoute, Req, Resp};

fn route(
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    description: &'static str,
) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "voice-lab",
        summary,
        description,
        tool: None,
        query: None,
        path_ints: &[],
        request: Req::None,
        response: Resp::NoContent,
        dialect: Dialect::Dashboard,
        endpoints: &[],
        model_task: None,
        confirm_note: None,
        writes: None,
        errors: &[],
        example: None,
    }
}

const CANDIDATE_MIME_TYPES: &[&str] = &[
    "audio/wav",
    "audio/mpeg",
    "audio/ogg",
    "audio/flac",
    "audio/aac",
    "application/octet-stream",
];

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            query: Some(|g| g.root_schema_for::<lab::VoiceModelQuery>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::VoiceModelFacts>()),
            model_task: Some(model_task::TTS),
            ..route(
                "GET",
                "/voice-lab/api/model",
                "What a text-to-speech model takes",
                "What the Voices lab's form offers for a text-to-speech alias, read from \
                 lmgw's own catalog and starting nothing: what a voice description does \
                 (none, a speaking style, the voice it designs, passed through) and whether \
                 the model needs one, whether it speaks a voice it is told, whether a seed \
                 reaches it, the languages it declares and what happens to the form's \
                 language, whether it clones a voice-library clip (for the cloning check), \
                 whether the GPU hold pauses it now and which alias answers in its place, and \
                 the licence its package states. A cloud alias's facts come from its \
                 capabilities.speech override; lmgw sends it no seed and no language. 400 for \
                 a blank alias or a language that is no language code, 404 for an unknown \
                 alias, 501 for one that is no OpenAI-shaped audio route (openai, openai_compatible or llama_cpp).",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::VoiceSentences>()),
            ..route(
                "GET",
                "/voice-lab/api/sentences",
                "List the sentence per language",
                "The sentence a round's candidates say, per language: the built-in one (the \
                 first two sentences of \"The North Wind and the Sun\", a phonetically rich \
                 passage) and the owner's saved one when there is one, which is what `text` \
                 holds then; and the line the cloning check says. A language the table lacks \
                 is listed once the owner saved a sentence for it.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<lab::VoiceSentenceSet>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::VoiceSentences>()),
            ..route(
                "POST",
                "/voice-lab/api/sentences",
                "Save the owner's sentence for a language",
                "Saves the sentence for the language, kept across restarts \
                 (<data_dir>/voice-lab/sentences.json), so it is the prefill from now on; an \
                 empty text drops it and the built-in sentence is the prefill again. Answers \
                 the whole table. 400 for a language that is no language code; 409 \
                 unreadable_file when sentences.json is there and is no JSON lmgw can read \
                 (it is never written over: nothing is saved until it is fixed or moved).",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::VoiceRounds>()),
            ..route(
                "GET",
                "/voice-lab/api/rounds",
                "List the kept rounds",
                "Every round kept under <data_dir>/voice-lab/rounds, newest first, with \
                 their candidates and what their audio takes on disk. Rounds stay until they \
                 are discarded; samples saved from them are copies and stay in the library.",
            )
        },
        DocRoute {
            tool: Some("lmgw__voice_generate"),
            request: Req::Json(|g| g.root_schema_for::<lab::VoiceGenerateArgs>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::VoiceRound>()),
            model_task: Some(model_task::TTS),
            ..route(
                "POST",
                "/voice-lab/api/rounds",
                "Render a round of candidates",
                "The voice_generate op as a route: the alias is opened once and says the \
                 text as `count` candidates (4 when absent, no cap), each with its own seed \
                 where the engine reads one; a configured fallback answers as for any \
                 request, and the round names who answered. The answer is the round as kept, \
                 each candidate with its seed, length and the path that plays it. Failures \
                 before the first candidate are the speech route's (the GPU hold with no \
                 fallback 503 gpu_hold, a voice-design model without a description 400 \
                 instructions_required, the engine's own error) with their status and code; \
                 a failure after it ends the round there (`stopped`). A caller that goes away \
                 (the request aborted, an MCP client hanging up) stops the round: the \
                 candidate in flight ends, the round keeps what it made with `stopped` set, \
                 and its request_logs row is a 200 canceled. The body size is not limited.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::VoiceRounds>()),
            ..route(
                "POST",
                "/voice-lab/api/rounds/clear",
                "Discard every round",
                "Deletes every kept round and its candidates' audio, and every directory \
                 under the rounds' that is no readable round (counted in `bytes`). A round \
                 still rendering is stopped and waited for first. Saved samples are copies in \
                 the voice library and stay. Answers the (empty) list.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::VoiceRounds>()),
            ..route(
                "POST",
                "/voice-lab/api/rounds/{id}/delete",
                "Discard a round",
                "Deletes the round and its candidates' audio; answers the rounds left. A \
                 round still rendering is stopped and waited for first, so nothing it writes \
                 after brings it back. 404 for a round that is not there, 400 for an id that \
                 is no round id.",
            )
        },
        DocRoute {
            path_ints: &["n"],
            response: Resp::Binary(CANDIDATE_MIME_TYPES),
            ..route(
                "GET",
                "/voice-lab/api/rounds/{id}/candidates/{n}",
                "Play a candidate",
                "The candidate's audio as the model answered it (a WAV for every local \
                 model), with its content type and Accept-Ranges: bytes. One Range \
                 (bytes=a-b, bytes=a- or bytes=-n) answers 206 with Content-Range; the answer \
                 carries an ETag made from the file (inode, length, modification time), and \
                 If-Range with it applies the range, any other If-Range serves the whole \
                 file, and so does a Range lmgw does not read; 416 \
                 range_not_satisfiable (Content-Range: bytes */size) for a range that names \
                 no byte of it. 404 for a round or candidate that is not there.",
            )
        },
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::VoiceSamples>()),
            ..route(
                "GET",
                "/voice-lab/api/samples",
                "List the voice library with how each sample was made",
                "The voice library's clips (the Audio lab's), each with the Voices lab's \
                 record of how it was made when it made it: alias, the model that answered, \
                 description, voice, seed, text, language, dates and licence. \
                 `provenance_current` is false for a clip replaced since (its SHA-256 no \
                 longer matches). Without an audio models directory the answer is a 200 \
                 with no samples and an `error` saying so; a samples.json lmgw cannot read \
                 leaves every record out and `records_error` says why.",
            )
        },
        DocRoute {
            tool: Some("lmgw__voice_sample_save"),
            request: Req::Json(|g| g.root_schema_for::<lab::VoiceSampleSaveArgs>()),
            response: Resp::Json(|g| g.root_schema_for::<lab::VoiceSampleSaved>()),
            confirm_note: Some("A sample of the same name is replaced only with overwrite."),
            ..route(
                "POST",
                "/voice-lab/api/samples",
                "Save a candidate as a sample",
                "The voice_sample_save op as a route: the candidate goes to the voice \
                 library as <name>.wav, leading and trailing silence cut and loudness \
                 untouched, with the text the engine was sent as its transcript, and how it \
                 was made is recorded. 409 name_taken when the library has an audio clip of \
                 that name and `overwrite` is not set (nothing is written); 409 \
                 unreadable_file when samples.json is no JSON lmgw can read (nothing is \
                 written); 404 for a round or candidate that is not \
                 there; 400 for a bad name or no audio models directory. A development \
                 gateway whose audio models directory is the installed app's refuses (400, \
                 code dev_shared_models_dir).",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<lab::VoiceCloneCheck>()),
            response: Resp::Binary(CANDIDATE_MIME_TYPES),
            dialect: Dialect::OpenAi,
            model_task: Some(model_task::TTS),
            writes: Some(false),
            ..route(
                "POST",
                "/voice-lab/api/clone-check",
                "Speak a test line with a sample",
                "Has a cloning text-to-speech model say a short line (the body's, else the \
                 language's check line) with a saved sample as its reference, through the \
                 same handler as POST /v1/audio/speech, whose answer is relayed as it came: \
                 the audio, x-lmgw-fallback when a fallback answered, or the route's OpenAI \
                 error envelope. With the class's voice directory on the library the sample \
                 is named as `voice`, as a realtime session names it; otherwise it goes as a \
                 voice_ref path with its transcript as reference_text. The check's own \
                 refusals (no alias, no line, no such sample) use the same envelope.",
            )
        },
    ]
}
