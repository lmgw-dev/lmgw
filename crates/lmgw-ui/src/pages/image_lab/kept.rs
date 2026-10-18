//! The Image lab's form and its kept groups (labs-store design §5): a
//! group's stored form put back into the page ("Open in form"), a preset's
//! prompt loaded, a kept picture sent to the edit panel by its artifact, the
//! upload an edit posts, which names a kept picture instead of carrying it,
//! and the group a request is kept in before it is run.

use gloo_net::http::Request;
use leptos::prelude::*;
use lmgw_api_types::image_lab::ImageLabAnswer;
use lmgw_api_types::image_lab::{ImageGenForm, ImageLabForm, EDIT_VARIANT};
use lmgw_api_types::lab_store::{LabFileRef, LabGroupView};

use super::super::lab_frame::LabView;
use super::Lab;
use crate::api;

impl Lab {
    /// Put `f` into the form: its model (when the list has it) and every
    /// field, the LoRA rows included. The model's own defaults are not
    /// applied over it.
    pub(super) fn load_form(self, f: &ImageGenForm) {
        let form = self.form;
        let model = f.model.trim().to_string();
        let listed = self
            .models
            .with_untracked(|v| v.iter().any(|m| m.name == model));
        if listed && model != self.alias.get_untracked() {
            // The alias effect would put the row's own defaults over the
            // fields set here.
            self.loaded.set_value(Some(model.clone()));
            self.alias.set(model);
        }
        form.prompt.set(f.prompt.clone());
        form.negative_prompt.set(f.negative_prompt.clone());
        form.width.set(f.width.clone());
        form.height.set(f.height.clone());
        form.n.set(f.n.clone());
        form.steps.set(f.steps.clone());
        form.cfg_scale.set(f.cfg_scale.clone());
        form.seed.set(f.seed.clone());
        form.sampler.set(f.sampler.clone());
        form.scheduler.set(f.scheduler.clone());
        form.output_format.set(f.output_format.clone());
        form.output_compression.set(f.output_compression.clone());
        let mut next = form.next_lora.get_untracked();
        let rows = f
            .loras
            .iter()
            .map(|r| {
                let id = next;
                next += 1;
                (id, r.path.clone(), r.multiplier.clone())
            })
            .collect();
        form.next_lora.set(next);
        form.loras.set(rows);
    }

    /// The picture or mask slot set to a kept file: no upload, the server
    /// sends its bytes.
    fn keep_slot(self, mask: bool, r: Option<LabFileRef>) {
        let f = self.form;
        let name = r.as_ref().map(|r| r.name.clone()).unwrap_or_default();
        if mask {
            f.mask_file.set_value(None);
            f.mask_ref.set(r);
            f.mask_name.set(name);
        } else {
            f.image_file.set_value(None);
            f.image_ref.set(r);
            f.image_name.set(name);
        }
    }

    /// "Open in form": the group's stored form, its route and, for an edit,
    /// its kept picture and mask.
    pub(super) fn open_in_form(self, g: &LabGroupView) {
        let form = match serde_json::from_value::<ImageLabForm>(g.form.clone()) {
            Ok(f) => f,
            Err(e) => {
                self.toasts
                    .err(format!("this ask's form does not read: {e}"));
                return;
            }
        };
        self.load_form(&form.form);
        let edit = g.plan.first().is_some_and(|c| c.variant == EDIT_VARIANT);
        self.editing.set(edit);
        self.keep_slot(false, form.image.filter(|_| edit));
        self.keep_slot(true, form.mask.filter(|_| edit));
        self.view.set(LabView::Form);
    }

    /// A kept picture into the edit panel's image slot.
    pub(super) fn send_to_edit(self, r: LabFileRef) {
        self.keep_slot(false, Some(r));
        self.editing.set(true);
        self.view.set(LabView::Form);
    }
}

/// The `FormData` an edit posts: the form as JSON (with a kept picture's or
/// mask's ref where no file is picked) plus the files picked. The canonical
/// multipart the edits route reads is assembled on the server — a browser
/// cannot name a part's `Content-Type` per file the way that route wants it,
/// and the page would be building a second copy of the request.
pub(super) fn build_upload(
    spec: &ImageGenForm,
    image: (Option<web_sys::File>, Option<LabFileRef>),
    mask: (Option<web_sys::File>, Option<LabFileRef>),
) -> Result<web_sys::FormData, String> {
    let form = web_sys::FormData::new().map_err(|_| "FormData unavailable".to_string())?;
    let lab_form = ImageLabForm {
        form: spec.clone(),
        image: image.1.filter(|_| image.0.is_none()),
        mask: mask.1.filter(|_| mask.0.is_none()),
    };
    if image.0.is_none() && lab_form.image.is_none() {
        return Err("pick an image to edit".into());
    }
    let json = serde_json::to_string(&lab_form).map_err(|e| e.to_string())?;
    let _ = form.append_with_str("form", &json);
    if let Some(file) = image.0 {
        let _ = form.append_with_blob_and_filename("image", &file, &file.name());
    }
    if let Some(file) = mask.0 {
        let _ = form.append_with_blob_and_filename("mask", &file, &file.name());
    }
    Ok(form)
}

/// Keep the form as a group (`POST /image-lab/api/groups`, or
/// `/groups/edit` with the edit's upload), nothing sent yet: the group, or
/// why not (an abort says "abort").
pub(super) async fn make_group(
    spec: &ImageGenForm,
    editing: bool,
    image: (Option<web_sys::File>, Option<LabFileRef>),
    mask: (Option<web_sys::File>, Option<LabFileRef>),
    signal: Option<&web_sys::AbortSignal>,
) -> Result<LabGroupView, String> {
    let sent = if editing {
        let form = build_upload(spec, image, mask)?;
        Request::post("/image-lab/api/groups/edit")
            .abort_signal(signal)
            .body(form)
            .map_err(|e| e.to_string())?
            .send()
            .await
    } else {
        Request::post("/image-lab/api/groups")
            .abort_signal(signal)
            .json(spec)
            .map_err(|e| e.to_string())?
            .send()
            .await
    };
    let resp = sent.map_err(|e| e.to_string())?;
    let resp = api::checked(resp).await.map_err(|e| refusal_text(&e))?;
    let text = resp.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("bad response body: {e}"))
}

/// A fetch the page aborted itself (Stop, or the page left).
pub(super) fn is_abort(e: &str) -> bool {
    e.to_lowercase().contains("abort")
}

/// A refusal of a call the page built itself, as the page shows it: the
/// envelope's code and message.
fn refusal_text(e: &crate::api::Error) -> String {
    match e {
        crate::api::Error::Api(err) if !err.code.is_empty() && err.code != "http_error" => {
            format!("{}: {}", err.code, err.message)
        }
        other => other.to_string(),
    }
}

/// Run kept group `id` (`POST /image-lab/api/groups/{id}/run`) and put the
/// answered group in its place. A Stop reads the group again until it is
/// no longer in flight, so what the route answered meanwhile still lands;
/// a refusal or a group not kept is the group's note.
pub(super) async fn run_group(lab: Lab, id: i64, signal: Option<&web_sys::AbortSignal>) {
    let sent = Request::post(&format!("/image-lab/api/groups/{id}/run"))
        .abort_signal(signal)
        .send()
        .await;
    let resp = match sent {
        Err(e) if is_abort(&e.to_string()) => None,
        Err(e) => {
            lab.history.note(id, format!("not sent: {e}"));
            None
        }
        Ok(resp) => match crate::api::checked(resp).await {
            Ok(resp) => Some(resp),
            Err(e) => {
                let why = refusal_text(&e);
                lab.toasts.err(why.clone());
                lab.history.note(id, format!("refused: {why}"));
                None
            }
        },
    };
    let Some(resp) = resp else {
        // Stopped, refused or cut off: whatever the gateway kept, once it
        // is no longer in flight.
        if let Err(e) = lab.history.settle(id).await {
            lab.toasts.err(format!("the kept ask did not load: {e}"));
        }
        return;
    };
    let text = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<ImageLabAnswer>(&text) {
        Ok(a) => {
            if let Some(e) = &a.store_error {
                lab.toasts.err(format!("not kept: {e}"));
                lab.history.note(id, format!("not kept: {e}"));
            }
            let group = a.group;
            lab.history.replace(group.clone());
            // Its group is gone from the list (a Clear while it ran): the
            // answer is still the user's, so its pictures stay on a card
            // above the history, saying why, until the next run.
            if !lab
                .history
                .groups
                .with_untracked(|v| v.iter().any(|g| g.id == id))
            {
                let why = match &a.store_error {
                    Some(e) => format!("not kept: {e}"),
                    None => "not kept: its group was deleted while the answer came".to_string(),
                };
                lab.stray.set(Some((group, why)));
            }
        }
        Err(e) => {
            lab.toasts.err(format!("bad response body: {e}"));
            let _ = lab.history.settle(id).await;
        }
    }
}
