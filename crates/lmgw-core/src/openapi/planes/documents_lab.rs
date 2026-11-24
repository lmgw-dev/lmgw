//! `/documents-lab/api/*`, the Documents lab (document-conversion design
//! WP12): the route that keeps a form and its files as a group, and the one
//! that turns a kept group into its `/v1/documents/convert` or
//! `/v1/convert/file` request, hands it to that handler and keeps the answer
//! as its run. The shapes are `lmgw-api-types`' `documents_lab` module.
//! Admin routes (`Cap::Admin`), so they appear in the admin document only.

use lmgw_api_types::documents_lab as lab;
use schemars::generate::SchemaGenerator;
use schemars::Schema;

use super::super::registry::{Dialect, DocRoute, Req, Resp};
use super::super::schemas;

fn route(
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    description: &'static str,
) -> DocRoute {
    DocRoute {
        method,
        path,
        tag: "documents-lab",
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

fn create_request(g: &mut SchemaGenerator) -> Schema {
    let form = g.subschema_for::<lab::DocumentsLabForm>().to_value();
    schemas::named(
        g,
        "DocumentsLabGroupRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["form"],
            "properties": {
                "form": {
                    "type": "string",
                    "contentMediaType": "application/json",
                    "description": "The form as a JSON string: the route (`native` or \
                        `docling`), the native options, and the files in request order. \
                        A file with id 0 is the next `file` part; any other id names a \
                        file the lab store keeps ({id, name}).",
                    "contentSchema": form
                },
                "file": {
                    "type": "array",
                    "items": { "type": "string", "format": "binary" },
                    "description": "One part per new file, in the order the form lists \
                        its files with id 0."
                }
            },
            "description": "multipart/form-data. The body size is not limited here; the \
                run holds the files to the route's max_body_mb."
        }),
    )
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            request: Req::Multipart(create_request),
            response: Resp::Json(|g| {
                g.root_schema_for::<lmgw_api_types::lab_store::LabGroupView>()
            }),
            dialect: Dialect::OpenAi,
            ..route(
                "POST",
                "/documents-lab/api/groups",
                "Keep a conversion's form and files as a group",
                "The dashboard's form and files kept in the lab store as a group of one \
                 cell (variant `native`, `async` or `docling`; model documents/convert or \
                 docling/v1/convert/file), answered with no run yet: nothing is sent. The \
                 files are the group's input artifacts in request order, and the stored \
                 form names each by its artifact id. A form that names no file, a file \
                 count that differs from the upload's parts, a kept file the store no \
                 longer has, options the chosen route cannot send (several page spans or \
                 a background job on the docling route) and a malformed upload are a 400 \
                 with code documents_lab_form; a store that cannot keep the group a 500 \
                 with code lab_store.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<lab::DocumentsLabAnswer>()),
            dialect: Dialect::OpenAi,
            ..route(
                "POST",
                "/documents-lab/api/groups/{id}/run",
                "Run a kept conversion",
                "The group's one cell: claimed (the group is in flight until its run is \
                 in), its multipart built from the stored form and the kept files (the \
                 native options as /v1/documents/convert's fields, or as the docling \
                 options lmgw maps onto them) and handed to that route's handler in the \
                 gateway under its max_body_mb, so the answer, its warnings and notes, the \
                 tool pool and the usage row are the route's own. The answer is 200 \
                 whatever the route said: the group, its run inside with the request as \
                 `request` (the fields as sent and each file part by name, file name, \
                 type, size and kept file `lab-artifact:<id>`), the route's status, code \
                 and message, its x-lmgw-* headers and its answer as text, each \
                 document's text (content, structure, docling's md_content and \
                 text_content) replaced by `lab-artifact:<id>`, the output artifact \
                 /labs/api/artifacts/{id} serves; docling's zip is kept whole and entry by \
                 entry. An async request answers at once with the group in flight and \
                 `job`, whose state is at /v1/documents/jobs/{job}; its run is the job's \
                 result once the job has ended (a cancelled job's run is its state with \
                 409 job_cancelled). No body. 404 not_found for a group that is not the \
                 Documents lab's, 409 cell_answered for one that has its run or is being \
                 answered, 500 cell_unbuilt for a stored form that no longer builds a \
                 request, 404 file_missing for a kept file whose bytes are gone.",
            )
        },
    ]
}
