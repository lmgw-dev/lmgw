//! `/image-lab/api/*`, the Image lab: the model list its form is built from,
//! the two routes that keep a form as a group, and the one that turns a kept
//! group into its `/v1/images/*` request, hands it to that handler and keeps
//! the answer as its run. The shapes are
//! `lmgw-api-types`' `image_lab` module, the types the handlers read and
//! write. Admin routes (`Cap::Admin`), so they appear in the admin document
//! only.

use lmgw_api_types::image_lab as lab;
use lmgw_api_types::openapi_ext::model_task;
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
        tag: "image-lab",
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

fn edit_request(g: &mut SchemaGenerator) -> Schema {
    let form = g.subschema_for::<lab::ImageLabForm>().to_value();
    schemas::named(
        g,
        "ImageLabEditRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["form"],
            "properties": {
                "form": {
                    "type": "string",
                    "contentMediaType": "application/json",
                    "description": "The form as a JSON string: the ImageGenForm the \
                        generate route takes as its body, with optionally `image` and `mask` \
                        naming kept pictures ({id, name}, an artifact the lab store keeps) \
                        in place of an upload.",
                    "contentSchema": form
                },
                "image": {
                    "type": "string",
                    "format": "binary",
                    "description": "The picture to edit; stands in for the form's `image` \
                        ref. One of the two is required."
                },
                "mask": {
                    "type": "string",
                    "format": "binary",
                    "description": "An optional mask."
                }
            },
            "description": "multipart/form-data. The body size is not limited."
        }),
    )
}

pub(crate) fn routes() -> Vec<DocRoute> {
    vec![
        DocRoute {
            response: Resp::Json(|g| g.root_schema_for::<lab::ImageLabModels>()),
            ..route(
                "GET",
                "/image-lab/api/models",
                "List the image models",
                "Everything that can answer an image request, with what the form needs to \
                 shape itself: the enabled local `image/<id>` rows and every cloud alias \
                 whose upstream catalog says it generates images, exactly as GET /v1/models \
                 publishes them (`edit` says whether the model serves \
                 /v1/images/edits). A local row also carries the flags its container is \
                 started with, which are the form's size, step and CFG defaults (the lab \
                 invents none), its container's state, and, once the container has been \
                 probed, the capabilities it reported: the sampler and scheduler lists, the \
                 real size bounds and the LoRAs on disk. `hold` says the GPU hold is on.",
            )
        },
        DocRoute {
            request: Req::Json(|g| g.root_schema_for::<lab::ImageGenForm>()),
            response: Resp::Json(|g| {
                g.root_schema_for::<lmgw_api_types::lab_store::LabGroupView>()
            }),
            dialect: Dialect::OpenAi,
            model_task: Some(model_task::IMAGE_GENERATION),
            ..route(
                "POST",
                "/image-lab/api/groups",
                "Keep a generation's form as a group",
                "The dashboard's structured form, checked to build the exact \
                 /v1/images/generations body and kept in the lab store as a group of one \
                 cell (variant `generate`), answered with no run yet: nothing is sent. The \
                 page then runs it (/image-lab/api/groups/{id}/run), so it holds the group, \
                 in flight, before the request goes out. The form's fields are the strings \
                 the inputs hold (an empty one says nothing about that setting); everything \
                 the OpenAI route does not read goes into sd.cpp's <sd_cpp_extra_args> \
                 block appended to the prompt. A form that is no request at all (no prompt, \
                 no model, a malformed number, only half of the size, a prompt carrying \
                 sd.cpp's own delimiters) is a 400 with code image_lab_form; a store that \
                 cannot keep the group a 500 with code lab_store. The body size is not \
                 limited.",
            )
        },
        DocRoute {
            request: Req::Multipart(edit_request),
            response: Resp::Json(|g| {
                g.root_schema_for::<lmgw_api_types::lab_store::LabGroupView>()
            }),
            dialect: Dialect::OpenAi,
            model_task: Some(model_task::IMAGE_EDIT),
            ..route(
                "POST",
                "/image-lab/api/groups/edit",
                "Keep an edit's form and pictures as a group",
                "As /image-lab/api/groups, for an edit (variant `edit`): the form and the \
                 pictures as parts of one multipart upload (`form` as JSON, `image`, \
                 optionally `mask`). The picture and the mask are the group's input \
                 artifacts; the form may name kept ones instead of uploading them \
                 (`image`, `mask`: {id, name}, a picture the store keeps, sent under the \
                 kept file's own name). A missing `form` or picture, a ref the store does \
                 not keep, a ref to an artifact that is no picture (its type named), a form \
                 that is no edit and a malformed upload are the same 400 as for a \
                 generation. Nothing is sent. The body size is not limited.",
            )
        },
        DocRoute {
            path_ints: &["id"],
            response: Resp::Json(|g| g.root_schema_for::<lab::ImageLabAnswer>()),
            dialect: Dialect::OpenAi,
            ..route(
                "POST",
                "/image-lab/api/groups/{id}/run",
                "Run a kept group",
                "The group's one cell: claimed (the group is in flight until its run is \
                 in), its request built from the stored form (an edit's multipart from its \
                 kept picture and mask, `model` among the fields) and handed to \
                 /v1/images/generations' or /v1/images/edits' handler in the gateway: the \
                 same guards, GPU hold, admission, model rewrite and logging, and the call \
                 appears in the request log. The answer is 200 whatever the route said: the \
                 group, its run inside with the request as `request` (a generation's body \
                 as sent; an edit's text fields as sent and each file part by name, file \
                 name, content type, size and kept file `lab-artifact:<id>`), the route's \
                 status, code and message, its x-lmgw-* headers (a hold fallback names \
                 itself there) and its answer as text, each `b64_json` picture replaced by \
                 `lab-artifact:<id>`, the output artifact /labs/api/artifacts/{id} serves. \
                 The answer is kept even when the client goes away once the route has \
                 answered; a client that goes away before leaves the group with no run. \
                 `store_error` says why a run was not kept (a full disk, the group deleted \
                 meanwhile): it is then there with no id, its pictures inline. No body. 404 \
                 not_found for a group that is not the Image lab's, 409 cell_answered for \
                 one that has its run or is being answered (a re-ask is a new group), 500 \
                 cell_unbuilt for a stored form that no longer builds a request, 404 \
                 file_missing for a kept picture whose file is gone.",
            )
        },
    ]
}
