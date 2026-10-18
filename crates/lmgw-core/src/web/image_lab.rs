//! Image lab: the "test the image routes" playground, the Audio lab's sibling
//! for stable-diffusion.cpp (image-generation design §8). The model picker,
//! the two panels and the kept history are the SPA's Image lab page; this
//! module is its server (`/image-lab/api/*`).
//!
//! Generation and editing are dispatched **in-process** through the very
//! handlers that serve `POST /v1/images/generations` and `/v1/images/edits`
//! ([`crate::proxy::handle_image_generation`] and `handle_image_edit`), so what
//! the page exercises is the real passthrough — the `resolve_image` guards, the
//! GPU hold, admission, the model rewrite, sd-server's two error bodies
//! normalized into the gateway's one envelope, and the `class = image` log row.
//! Only the auth middleware and the HTTP hop are skipped, the same trade the
//! Audio lab and `local_model_test` make.
//!
//! Every dispatch is kept in the lab store (labs-store design §5): the page
//! makes a group of one cell from its form first (`/image-lab/api/groups`,
//! `/groups/edit`), then runs it (`/groups/{id}/run`), and the run is the
//! route's answer with each picture an output artifact. An edit's picture
//! and mask are the group's input artifacts.
//!
//! The request itself is **not** built here: `ImageGenForm::generation_body`
//! lives in `lmgw-api-types` because the page's "Request" panel has to show the
//! document this module dispatches, byte for byte. What is built here is the
//! multipart — a browser upload arrives as two files plus a JSON form, and the
//! edits route wants one `multipart/form-data` body with `model` in it.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::image_lab::{
    ImageLabModel, ImageLabModels, EDITS_ENDPOINT, GENERATIONS_ENDPOINT,
};

use crate::capabilities::exposed::exposed_entries;
use crate::state::SharedState;

/// `POST /image-lab/api/groups`, `/groups/edit` and `/groups/{id}/run`: the
/// form kept as a group, then turned into the request, dispatched and kept
/// as its run.
mod dispatch;
/// Recording a dispatch's answer as the group's run, and the answer.
mod kept;
/// The multipart body an edit is relayed as.
mod multipart;
/// A generation's pictures found in the route's answer.
mod outputs;

pub use dispatch::{create_edit, create_generate, run};

// ---------------------------------------------------------------------------
// Model list
// ---------------------------------------------------------------------------

/// `GET /image-lab/api/models` — everything that can answer an image request,
/// with what the form needs to shape itself.
///
/// Two populations, one list, exactly as the routes see them (§5): the enabled
/// `image/<id>` rows, and every cloud alias whose upstream catalog says it
/// generates images. Both arrive from [`exposed_entries`] — the same join
/// `/v1/models` publishes — so "does this model serve `/v1/images/edits`" is
/// answered here by the capability object the route itself is gated on, not by
/// a second opinion.
///
/// A local row carries three things a cloud alias has not got: the `args` its
/// container was started with (which is where the size defaults come from — the
/// lab invents none), its container's state, and, once that container has been
/// probed, the **capabilities** it reported: the sampler and scheduler lists,
/// the real size bounds, the LoRAs on disk. `None` there is why the page offers
/// a free-text sampler field rather than an empty select.
pub async fn list_models(State(state): State<SharedState>) -> Response {
    let snap = state.snapshot();
    let runtime: Vec<_> = state
        .runtime()
        .list()
        .into_iter()
        .filter(|v| v.class == crate::runtime::Class::Image)
        .collect();

    // The gateway's own probed capabilities; the document's mirror is
    // `ImageCapabilities` (lmgw-api-types).
    let mut models: Vec<ImageLabModel<crate::runtime::image::ImageCapabilities>> = Vec::new();
    for e in exposed_entries(&state).await {
        let Some(caps) = e.capabilities.as_ref() else {
            continue;
        };
        if !caps.endpoints.iter().any(|p| p == GENERATIONS_ENDPOINT) {
            continue;
        }
        let row = snap
            .enabled_image_models()
            .find(|m| snap.image_public_name(&m.model_id) == e.name);
        let rt = row.and_then(|m| runtime.iter().find(|r| r.model_id == m.model_id));
        models.push(ImageLabModel {
            edit: caps.endpoints.iter().any(|p| p == EDITS_ENDPOINT),
            local: row.is_some(),
            model_id: row.map(|m| m.model_id.clone()),
            task: caps.task.clone(),
            endpoints: caps.endpoints.clone(),
            // The operator's own words about the pipeline, and the flags its
            // argv carries — `width`, `height`, `steps`, `cfg_scale`,
            // `sampling_method` are the form's defaults when the row sets them.
            modes: row.map(|m| m.modes()),
            args: row.map(|m| m.args.clone()),
            notes: e.notes,
            // Runtime facts. Absent for a cloud alias, which has no container.
            state: rt.map(|r| r.state.as_str().to_string()),
            warnings: rt.map(|r| r.warnings.clone()).unwrap_or_default(),
            image_capabilities: rt.and_then(|r| r.image_capabilities.clone()),
            name: e.name,
            owner: e.owner,
        });
    }
    Json(ImageLabModels {
        models,
        models_dir: snap.settings.image.models_dir.clone(),
        hold: snap.settings.hold.active,
    })
    .into_response()
}
