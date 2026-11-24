//! `POST /v1/documents/convert` (document-conversion design §7.1, §7.2,
//! §7.6): lmgw's own route, multipart or JSON.

use lmgw_api_types::documents::convert::{
    ConvertAnswer, ConvertOptions, ConvertRequest, ConvertedDocument,
};
use lmgw_api_types::documents::jobs::{ConvertJob, JobAccepted, JobResultQuery};
use lmgw_api_types::openapi_ext::endpoint_group;
use schemars::{Schema, SchemaGenerator};
use serde_json::{json, Value};

use super::super::super::registry::{Dialect, DocRoute, Req, Resp};
use super::super::super::schemas;
use super::base;

/// The multipart form: one `file` part per file, the options as fields.
fn form(g: &mut SchemaGenerator) -> Schema {
    let options = g.subschema_for::<ConvertOptions>();
    schemas::named(
        g,
        "DocumentsConvertForm",
        schemars::json_schema!({
            "allOf": [
                options,
                {
                    "type": "object",
                    "required": ["file"],
                    "properties": {
                        "file": {
                            "type": "array",
                            "items": {"type": "string", "format": "binary"},
                            "description": "One part named file per file, in the order the \
                                answer lists them; its filename is the entry's name."
                        }
                    }
                }
            ],
            "description": "multipart/form-data: one file part per file, and the options as \
                text fields (true or false for the switches, document_timeout as a number \
                of seconds, ocr_languages as codes separated by commas, + or spaces)."
        }),
    )
}

fn example() -> Value {
    json!({
        "files": [
            {"name": "example-a.txt", "data_base64": "IyBFeGFtcGxlCgpIZWxsby4="}
        ],
        "output": "markdown",
        "pages": "1-3"
    })
}

/// The sync answer, or an async request's 202 (documented as the second
/// shape beside the first).
fn answer_or_job(g: &mut SchemaGenerator) -> Schema {
    let a = g.subschema_for::<ConvertAnswer>();
    let j = g.subschema_for::<JobAccepted>();
    schemas::named(
        g,
        "DocumentsConvertAnswerOrJob",
        schemars::json_schema!({
            "oneOf": [a, j],
            "description": "The answer (200, or the one file's error status), or with async \
                true the job (202)."
        }),
    )
}

fn result_or_entry(g: &mut SchemaGenerator) -> Schema {
    let a = g.subschema_for::<ConvertAnswer>();
    let e = g.subschema_for::<ConvertedDocument>();
    schemas::named(
        g,
        "DocumentsJobResult",
        schemars::json_schema!({
            "oneOf": [a, e],
            "description": "The whole answer, or with ?index= one file's entry."
        }),
    )
}

fn jobs() -> Vec<DocRoute> {
    vec![
        DocRoute {
            description: "An async conversion job's state: running, succeeded (every file has \
                an outcome and one was converted), failed (none was), cancelled or \
                interrupted (the gateway stopped while it ran; files that had finished keep \
                their results). Per file its state, parts and error code; parts and \
                parts_done grow as files finish (progress inside one file is not reported \
                yet). While it runs, pool says how many tool runs of all callers run and wait \
                in the shared tool pool. A job another key made is a 404 job_not_found; the \
                owner sees every job. Ended jobs are kept for Settings › Retention › \
                Conversion jobs (documents_job_retention_days, 0 keeps them).",
            response: Resp::Json(|g| g.root_schema_for::<ConvertJob>()),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            ..base(
                "GET",
                "/v1/documents/jobs/{id}",
                "lmgw-inference",
                "A conversion job's state",
            )
        },
        DocRoute {
            description: "The ended job's answer, the same body the request would have had \
                sync (with output json, each entry's structure and a null content); a file without an outcome (the gateway stopped first) is a failed entry \
                with interrupted. ?index=<n> answers one file's entry as soon as that file \
                has one, while the rest still run (409 file_pending before). A running job \
                without ?index= is 409 job_running, a cancelled one 409 job_cancelled (its \
                results were deleted).",
            query: Some(|g| g.root_schema_for::<JobResultQuery>()),
            response: Resp::Json(result_or_entry),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            ..base(
                "GET",
                "/v1/documents/jobs/{id}/result",
                "lmgw-inference",
                "A conversion job's result",
            )
        },
        DocRoute {
            description: "Cancels a running job: its running tools are stopped with their \
                process groups, their pool slots freed, and its stored results deleted; the \
                answer is its state, cancelled. An ended job is deleted with its results, and \
                the answer is its state as it was.",
            response: Resp::Json(|g| g.root_schema_for::<ConvertJob>()),
            dialect: Dialect::OpenAi,
            endpoints: &[endpoint_group::OTHER],
            writes: Some(true),
            ..base(
                "DELETE",
                "/v1/documents/jobs/{id}",
                "lmgw-inference",
                "Cancel or delete a conversion job",
            )
        },
    ]
}

pub(super) fn routes() -> Vec<DocRoute> {
    let mut all = jobs();
    all.push(DocRoute {
        description: "Turns one file or several into text a model can read — lmgw's own \
            route. Each file's format is read from its bytes and resolved through the same \
            table Settings › Documents shows: text as it is, Word/PowerPoint/Excel/\
            OpenDocument files through lmgw's own reader (headings, lists and tables kept), \
            PDFs through poppler's geometry when poppler-utils is installed (headings by \
            font size and weight, columns in reading order, repeated page headers and \
            footers left out with a header_removed warning, aligned cells as tables; poppler \
            reads the span from the first to the last page asked for, and the pages asked \
            for are answered). With the layout model switched on under Settings › Documents \
            (off until then; its file is downloaded there or by doc_model_add), a PDF's \
            pages with text are laid out from the regions the model finds on the processor: \
            paragraphs, headings, list items, tables, figures (drawn ones too; source onnx), \
            captions, formulas and code, the text inside pictures left out with a \
            picture_text warning; a page it could not read is laid out as without it, with \
            a part_failed warning. output json answers each file's structure in structure \
            (content null): parts (pages with width_pt/height_pt, slides, sheets, or one \
            document) holding blocks in reading order, each with its kind, level, text, \
            bbox in points from the page's top-left corner (null without geometry), source \
            (text_layer, native) and, for a table, its cells. The answer \
            always has one documents entry per file in request order, each with its status \
            (converted, partial, failed), content, warnings and stats; one bad file never \
            fails the others. The status is 200 when a file was converted or partly \
            converted; otherwise a one-file request answers that file's code (415 \
            tool_missing, no_reader or unsupported, 422 for an unreadable file or \
            memory_limit; where \
            nothing but the page-reading model could read its pages: 503 model_held, 403 \
            model_not_allowed, 502 model_failed) and a several-file request 422, with the \
            same body. Every gap is a warning with a code: \
            scanned PDF pages and pictures are read by text recognition (tesseract; ocr \
            auto, force or off; ocr_languages, at most 32 codes of letters, digits and _, \
            else a 400 invalid_option; stats.parts_by_source.ocr counts them), a page \
            not recognised is no_reader or tool_missing, languages without their pack one \
            ocr_language_missing, a page tesseract was unsure of ocr_low_confidence, a page \
            turned upright before it was read ocr_page_turned, a poppler run that needed \
            more than its memory limit and was run again under the wide one \
            memory_limit_widened, a JPEG 2000 image poppler drew blank image_not_decoded; \
            HTML, EPUB, e-mail (.eml, \
            attachments read like files of their own, each counted against the output \
            bound as it is read) and legacy .xls are read by lmgw's own reader program, a \
            child process with a timeout and a memory limit (tools lists it as reader; a file \
            that passes a limit fails alone, by the limit's name); old .doc and .ppt files \
            are read through LibreOffice as the docx or pptx it writes, RTF, LaTeX, \
            reStructuredText, Org, MediaWiki, DocBook and FictionBook through pandoc \
            (sandboxed; without it the text markups stay text with a tool_missing warning), \
            a docx with equations through pandoc with the equations as LaTeX (without it \
            lmgw's reader, equations_dropped); audio, and the sound of videos through \
            ffmpeg, is transcribed by the speech-to-text model chosen under Settings › \
            Documents as the calling key (none chosen: model_not_chosen, 415; the key may \
            not use it: model_not_allowed, 403; held without a fallback: model_held, 503; \
            the model failed: model_failed, 502; source asr, stats.model_calls 1 and \
            stats.models naming who answered, files_transcribed 1; the call is part of the file's \
            document_timeout, one usage row of documents-asr, a call dropped before its \
            answer a 499 row); document_timeout (seconds, counted from a file's first tool-pool step, the time its steps work, not the \
            time they wait) fails a slower file with deadline_exceeded and stops its tools, \
            the other files go on (a file whose page-reading model stage it cuts keeps the \
            pages read so far and is partial); an option whose feature is not built yet is \
            a request-wide not_yet warning. Pictures (pictures placeholder, embedded or referenced; \
            picture_min_area; images_scale) are in each entry's pictures list in reading \
            order, with their bytes for referenced (and embedded with output json); figure \
            blocks name them in json; pictures_left_out and pictures_partial warnings count \
            what was left out or kept without its picture. model_help sends PDF pages (not \
            picture files yet) to the page-reading model chosen under Settings › Documents \
            (none chosen: a model_not_chosen warning for a value other than off, and no \
            model is ever called), after the text layer and text recognition: absent or \
            missing_or_tables the pages nothing read, the pages text recognition read \
            poorly and the pages with a table; missing only the pages nothing read \
            (neither a text layer nor text recognition); every_page all of them; off none. \
            Once a model is chosen a request that does not set model_help uses it; only \
            model_help off, or no chosen model, guarantees that no model is called. A page \
            the model read has the source vision and replaces what the tools read there \
            (its pictures stay); stats.model_calls and stats.models say which model \
            answered how many calls (answered_for when a fallback answered for the chosen \
            model, which it does whenever that model cannot run) and the bound on calls in \
            flight (the model's slots); a failed file keeps both. A page the model could \
            not read keeps what the tools read, with a warning naming it: model_held (the \
            GPU hold, no fallback), model_cannot_see (the fallback that answers takes no \
            pictures; nothing is sent to it), model_not_allowed (the key's scope, budget, \
            tokens-per-minute window or expiry) or part_failed (an answer that ran to its \
            token limit, a refusal, any finish but a stop, an upstream error, or the \
            caller's document_timeout: never part of the text). A page larger than \
            PAGE_MAX_PIXELS at 150 dpi is sent scaled down, with a page_scaled warning. The \
            calls are made as the request's key, one usage row each (documents-page; a call \
            dropped before its answer is a 499 row); a key's budget can be passed by at most \
            the calls in flight, each up to the model's largest answer. describe_pictures \
            sends each exported picture of at least describe_min_area of its page (default \
            0.05, docling's; a smaller one is kept undescribed and counted in \
            stats.pictures_below_describe_min_area; PNG or JPEG of at most \
            PAGE_MAX_PIXELS, others a not_yet warning) to the picture-description model chosen \
            under Settings › Documents (none chosen: a model_not_chosen warning, pictures kept \
            without a description), with picture_prompt (default: docling's \
            \"Describe this image in a few sentences.\") and the picture's caption and the \
            paragraph before it as context, each cut at PICTURE_CONTEXT_MAX_BYTES (a \
            picture_context_cut warning); the description is the picture's description in \
            the pictures list and its figure block, a paragraph after the picture and its \
            caption in markdown, and its text after the caption in output text, in every \
            pictures mode. A picture_prompt that by itself fills the model's context (counted \
            once per request) sends no picture, with one part_failed warning. Its \
            calls go the page-reading model's way (the key's policy, the hold and its \
            fallback, model_cannot_see, the model's slots as the bound, one usage row each, \
            documents-picture, counted in stats.models as pictures_described); a picture \
            not described keeps its picture with a warning (model_held, model_cannot_see, \
            model_not_allowed, part_failed) and the file is partial, never failed for it. \
            async true answers 202 with a job ({\"job\": \"cj_…\"}, Location header) \
            instead of waiting: its state, result and cancel are under \
            /v1/documents/jobs/{id}. The body is bounded by max_body_mb, which bounds how many files \
            one request carries; there is no other file-count or page limit. The converted \
            output of all a request's files together is bounded by MAX_UNCOMPRESSED_BYTES \
            (1 GiB): the file that crosses it fails with output_too_large (413 for a \
            one-file request), files already converted stay converted. A file's reading and \
            shaping run in the same pool; a request without a key takes its turn as a \
            caller of its own. An option that changes nothing (page_break on a file without \
            pages, compact_tables with output text) is an option_not_applicable warning. \
            Tool runs share \
            the gateway-wide tool pool (Settings › Documents, Parallel tool runs), taken \
            round robin per key. A client that goes away stops its conversion and its tools. \
            Each request writes one usage row (documents, documents/convert).",
        request: Req::MultipartOrJson {
            multipart: form,
            json: |g| g.root_schema_for::<ConvertRequest>(),
        },
        response: Resp::Json(answer_or_job),
        dialect: Dialect::OpenAi,
        endpoints: &[endpoint_group::OTHER],
        writes: Some(false),
        example: Some(example),
        ..base(
            "POST",
            "/v1/documents/convert",
            "lmgw-inference",
            "Convert documents to text",
        )
    });
    all
}
