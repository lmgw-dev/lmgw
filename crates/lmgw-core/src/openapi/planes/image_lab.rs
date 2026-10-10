//! `/image-lab/api/*`, the Image lab: the model list its form is built from,
//! and the two routes that turn the form into a `/v1/images/*` request and
//! hand it to that handler. The shapes are `lmgw-api-types`' `image_lab`
//! module, the types the handlers read and write. Admin routes
//! (`Cap::Admin`), so they appear in the admin document only.

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
        example: None,
    }
}

fn edit_request(g: &mut SchemaGenerator) -> Schema {
    let form = g.subschema_for::<lab::ImageGenForm>().to_value();
    schemas::named(
        g,
        "ImageLabEditRequest",
        schemars::json_schema!({
            "type": "object",
            "required": ["form", "image"],
            "properties": {
                "form": {
                    "type": "string",
                    "contentMediaType": "application/json",
                    "description": "The form as a JSON string: an ImageGenForm, the same \
                        document the generate route takes as its body.",
                    "contentSchema": form
                },
                "image": {
                    "type": "string",
                    "format": "binary",
                    "description": "The picture to edit."
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
            response: Resp::Json(|g| g.root_schema_for::<lab::ImageGenerateResult>()),
            dialect: Dialect::OpenAi,
            model_task: Some(model_task::IMAGE_GENERATION),
            writes: Some(false),
            ..route(
                "POST",
                "/image-lab/api/generate",
                "Generate an image from the form",
                "The dashboard's structured form, turned into the exact \
                 /v1/images/generations body and handed to that route's handler in the \
                 gateway: the same guards, GPU hold, admission, model rewrite and logging, \
                 and the call appears in the request log. Where /v1/images/generations takes \
                 the OpenAI body, this takes the form, whose fields are the strings the \
                 inputs hold (an empty one says nothing about that setting); everything the \
                 OpenAI route does not read goes into sd.cpp's \
                 <sd_cpp_extra_args> block appended to the prompt. A success is wrapped: \
                 `request` is the body that was built and sent, `headers` the x-lmgw-* \
                 headers the route stamped (a hold fallback names itself there), `response` \
                 the route's answer. A failure is not wrapped: the route's own status and \
                 OpenAI-shaped error come back as they were. A form that is no request at all \
                 (no prompt, no model, a malformed number, only half of the size, a prompt \
                 carrying sd.cpp's own delimiters) is a 400 with code image_lab_form. The \
                 body size is not limited.",
            )
        },
        DocRoute {
            request: Req::Multipart(edit_request),
            response: Resp::Json(|g| g.root_schema_for::<lab::ImageEditResult>()),
            dialect: Dialect::OpenAi,
            model_task: Some(model_task::IMAGE_EDIT),
            writes: Some(false),
            ..route(
                "POST",
                "/image-lab/api/edit",
                "Edit an image from the form",
                "As /image-lab/api/generate, for /v1/images/edits. The route takes the \
                 form and the pictures as parts of one multipart upload (`form` as JSON, \
                 `image`, optionally `mask`) and builds the single multipart request \
                 /v1/images/edits accepts, with the form's text fields and `model` in it. \
                 `request` in the answer is a summary of what was sent: the text fields as \
                 sent and each file part by name, file name, content type and size. A \
                 missing `form` or `image`, a form that is no request, a malformed upload \
                 and a model whose edit pipeline is off are the same 400 or the /v1 route's \
                 own refusal as for generate. The body size is not limited.",
            )
        },
    ]
}
