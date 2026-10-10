//! The Chat API's attachment and export routes, under the "Chat" tag and
//! merged into the Chat plane (`planes::chat`): the upload, a draft's
//! delete, mode and new transcription, the stored bytes and text, and the
//! exports of a thread, a folder and the whole Chat. Every shape is
//! `lmgw-api-types`' (`chat_attachments`, `chat_export`, `chat_threads`), the
//! types the handlers read and write.

use lmgw_api_types::chat;
use lmgw_api_types::chat_attachments as att;
use lmgw_api_types::chat_export as export;
use lmgw_api_types::chat_threads::AttachmentMeta;

use super::super::registry::{DocRoute, Req, Resp};
use super::chat::chat_route;

fn export_query(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    g.root_schema_for::<export::ExportQuery>()
}

fn bundle(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    g.root_schema_for::<export::ChatExport>()
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            path_ints: &["id"],
            query: Some(export_query),
            response: Resp::Download(&[
                ("text/markdown; charset=utf-8", None),
                ("application/json", Some(bundle)),
            ]),
            ..chat_route(
                "GET",
                "/chat/api/threads/{id}/export",
                "Export a thread",
                "The thread as one file. format=md (the default) is a readable Markdown \
                 transcript, text/markdown; charset=utf-8, with its attachments listed by name; \
                 format=json is the lossless lmgw.chat.v1 file, application/json, with every \
                 thread column, every message, and each attachment's metadata, extracted text \
                 and base64 bytes (the `format` field of the file says lmgw.chat.v1). The \
                 answer carries Content-Disposition: attachment with a file name \
                 lmgw-chat-{id}-{title}.md or .json (a temporary thread's id reads temp{n}) and \
                 Cache-Control: no-store. A temporary thread is exported from memory. A format \
                 other than md, markdown or json is a 400 bad_request; a thread that is not \
                 there, or not the caller's to see, is a 404 not_found.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            query: Some(export_query),
            response: Resp::Download(&[("application/zip", None)]),
            ..chat_route(
                "GET",
                "/chat/api/folders/{id}/export",
                "Export a folder",
                "A zip with one file per stored thread of the folder, in the format asked for \
                 (md or json, as for one thread), and a README.txt. archived=0 takes the active \
                 threads, 1 the archived ones, all (the default) both; another value is a 400 \
                 bad_request, as is a format other than md, markdown or json. The answer carries \
                 Content-Disposition: attachment (lmgw-folder-{name}-{date}.zip), Content-Length \
                 and Cache-Control: no-store; it is as large as the folder is and not capped. \
                 Temporary threads are not part of a zip. A device key's zip leaves out Admin \
                 Chat threads; a folder that is not there, or not the caller's to see, is a 404 \
                 not_found.",
            )
        },
        DocRoute {
            query: Some(export_query),
            response: Resp::Download(&[("application/zip", None)]),
            ..chat_route(
                "GET",
                "/chat/api/export",
                "Export every chat",
                "A zip with one file per stored thread, in the format asked for (md or json, as \
                 for one thread), a directory per folder named after it, and a README.txt. \
                 archived=0 takes the active threads, 1 the archived ones, all (the default) \
                 both; another value is a 400 bad_request, as is a format other than md, \
                 markdown or json. The answer carries Content-Disposition: attachment \
                 (lmgw-chats-{date}.zip), Content-Length and Cache-Control: no-store; it is as \
                 large as the Chat is and not capped. Temporary threads are not part of a zip. \
                 A device key's zip leaves out Admin Chat threads.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            query: Some(|g| g.root_schema_for::<att::UploadQuery>()),
            request: Req::Raw("application/octet-stream"),
            response: Resp::Json(|g| g.root_schema_for::<AttachmentMeta>()),
            ..chat_route(
                "POST",
                "/chat/api/threads/{id}/attachments",
                "Upload an attachment",
                "The body is the file's bytes, not multipart; ?name= is its name. The kind is \
                 sniffed from the bytes (image, text, pdf, office, audio), never taken from the \
                 name or the Content-Type. The file is stored as a draft of the thread, to be \
                 sent with the next message (`attachments` of POST /chat/api/threads/{id}/send), \
                 and answered as its metadata, with `blockers` and `hints` saying what the \
                 thread's current model cannot take. A PDF is read at upload (text-class, \
                 scanned or hybrid in `meta.class`; a text-class one starts with the Chat's PDF \
                 mode, or none to ask), an office file likewise, and an audio file is \
                 transcribed at once when its model cannot hear it and a speech-to-text model \
                 is set (a failure is `meta.transcript_error`, not a refusal). The body is \
                 bounded by max_body_mb (413 body_limit naming it, for a declared or a \
                 chunked body). 400 empty_attachment for an empty file, 404 not_found for a \
                 thread that is not there or not the caller's, 415 unsupported_attachment for \
                 bytes of no supported kind, 422 extract_failed (the file could not be read) \
                 or tool_missing (the PDF reader is not installed).",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<chat::Ack>()),
            ..chat_route(
                "POST",
                "/chat/api/attachments/{id}/delete",
                "Delete a draft attachment",
                "Removes an attachment that was not sent yet. 404 not_found for one that is \
                 not there or not the caller's; 409 attachment_sent once it was sent with a \
                 message (it belongs to that message then).",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Download(&[("*/*", None)]),
            ..chat_route(
                "GET",
                "/chat/api/attachments/{id}",
                "Read an attachment's bytes",
                "The stored file as uploaded, with the media type the bytes were sniffed as \
                 (image/png, application/pdf, audio/wav, ...); a text file is served as \
                 text/plain; charset=utf-8. The answer carries X-Content-Type-Options: nosniff \
                 and Content-Security-Policy: sandbox, so a file opened in a browser cannot run \
                 anything. 404 not_found for an attachment that is not there or not the \
                 caller's.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            request: Req::Json(|g| g.root_schema_for::<att::ModeRequest>()),
            response: Resp::Json(|g| g.root_schema_for::<att::ModeSet>()),
            ..chat_route(
                "POST",
                "/chat/api/attachments/{id}/mode",
                "Choose how a PDF is sent",
                "Sets how a PDF whose every page has text goes to the model: mode text sends \
                 the extracted text, images sends the pages as images. Drafts only. 400 \
                 bad_request for another mode, 404 not_found for an attachment that is not \
                 there or not the caller's, 409 attachment_sent once it was sent, 422 \
                 mode_not_applicable for any file but a text-class PDF (scanned and hybrid \
                 ones, and every other kind, are sent automatically).",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<att::Transcribed>()),
            ..chat_route(
                "POST",
                "/chat/api/attachments/{id}/transcribe",
                "Transcribe an audio draft again",
                "Runs the speech-to-text call again for an audio draft, after it failed at \
                 upload or once a model is set, and answers the attachment's new `meta` (the \
                 transcript's alias and token estimate in place of `transcript_error`). The \
                 body is empty. 404 not_found for an attachment that is not there or not the \
                 caller's, 409 attachment_sent once it was sent, 422 not_audio for any other \
                 kind, 422 stt_not_set when no speech-to-text model is set for the thread, 422 \
                 transcription_failed with the call's own message.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Download(&[("text/plain; charset=utf-8", None)]),
            ..chat_route(
                "GET",
                "/chat/api/attachments/{id}/text",
                "Read what the model reads of an attachment",
                "A text file as it is, a PDF's or office file's extracted text, or an audio \
                 file's transcript, as text/plain; charset=utf-8, with X-Content-Type-Options: \
                 nosniff and Content-Security-Policy: sandbox. 404 not_found for an attachment \
                 that is not there or not the caller's, and 404 no_text for an image, or an \
                 audio file with no transcript yet.",
            )
        },
    ]
}
