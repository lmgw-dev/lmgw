//! The Documents lab's calls (WP12): the upload a group is made from (the
//! form as JSON, a `file` part per new file, kept files named by their
//! artifact), the run, and a kept group put back into the form ("Open in
//! form": its options and its files as kept files, so a re-run with changed
//! options uploads nothing again).

use gloo_net::http::Request;
use leptos::prelude::*;
use lmgw_api_types::documents_lab::{DocumentsLabAnswer, DocumentsLabForm, DocumentsLabSettings};
use lmgw_api_types::lab_store::{LabFileRef, LabGroupView};

use super::super::lab_frame::LabView;
use super::form::{kind_of_mime, Picked};
use super::Lab;
use crate::api;

/// A fetch the page aborted itself (Stop, or the page left).
pub(super) fn is_abort(e: &str) -> bool {
    e.to_lowercase().contains("abort")
}

/// A refusal of a call the page built itself: the envelope's code and
/// message.
fn refusal_text(e: &crate::api::Error) -> String {
    match e {
        crate::api::Error::Api(err) if !err.code.is_empty() && err.code != "http_error" => {
            format!("{}: {}", err.code, err.message)
        }
        other => other.to_string(),
    }
}

/// The `FormData` a group is made from: the form, its files in order (a new
/// one with id 0 and its bytes as the next `file` part, a kept one by its
/// artifact).
pub(super) fn build_upload(
    settings: &DocumentsLabSettings,
    files: &[(Picked, Option<web_sys::File>)],
) -> Result<web_sys::FormData, String> {
    let data = web_sys::FormData::new().map_err(|_| "FormData unavailable".to_string())?;
    let form = DocumentsLabForm {
        settings: settings.clone(),
        files: files
            .iter()
            .map(|(p, _)| {
                p.kept.clone().unwrap_or(LabFileRef {
                    id: 0,
                    name: p.name.clone(),
                })
            })
            .collect(),
    };
    let json = serde_json::to_string(&form).map_err(|e| e.to_string())?;
    let _ = data.append_with_str("form", &json);
    for (p, file) in files {
        if p.kept.is_some() {
            continue;
        }
        let Some(file) = file else {
            return Err(format!("{} is no longer here: add it again", p.name));
        };
        let _ = data.append_with_blob_and_filename("file", file, &file.name());
    }
    Ok(data)
}

/// Keep the form and its files as a group (`POST /documents-lab/api/groups`),
/// nothing sent yet.
pub(super) async fn make_group(
    data: web_sys::FormData,
    signal: Option<&web_sys::AbortSignal>,
) -> Result<LabGroupView, String> {
    let resp = Request::post("/documents-lab/api/groups")
        .abort_signal(signal)
        .body(data)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = api::checked(resp).await.map_err(|e| refusal_text(&e))?;
    let text = resp.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("bad response body: {e}"))
}

/// Run kept group `id` and put the answered group in its place. An async
/// run answers at once with its job: the job is the card's to follow, and
/// the group is read again until its run is in. A Stop reads the group
/// again until it is no longer in flight; a refusal is the group's note.
pub(super) async fn run_group(lab: Lab, id: i64, signal: Option<&web_sys::AbortSignal>) {
    let sent = Request::post(&format!("/documents-lab/api/groups/{id}/run"))
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
    let answer = match resp {
        Some(resp) => {
            let text = resp.text().await.unwrap_or_default();
            serde_json::from_str::<DocumentsLabAnswer>(&text).map_err(|e| e.to_string())
        }
        None => Err(String::new()),
    };
    match answer {
        Ok(a) => {
            if let Some(e) = &a.store_error {
                lab.toasts.err(format!("not kept: {e}"));
                lab.history.note(id, format!("not kept: {e}"));
            }
            if let Some(job) = &a.job {
                lab.jobs.update(|j| {
                    j.insert(id, job.clone());
                });
            }
            let waits = a.job.is_some();
            lab.history.replace(a.group);
            if waits {
                if let Err(e) = lab.history.settle(id).await {
                    lab.toasts.err(format!("the kept run did not load: {e}"));
                }
            }
        }
        Err(e) => {
            if !e.is_empty() {
                lab.toasts.err(format!("bad response body: {e}"));
            }
            if let Err(e) = lab.history.settle(id).await {
                lab.toasts.err(format!("the kept run did not load: {e}"));
            }
        }
    }
}

impl Lab {
    /// "Open in form": the group's options and its files, as kept files.
    pub(super) fn open_in_form(self, g: &LabGroupView) {
        let form = match serde_json::from_value::<DocumentsLabForm>(g.form.clone()) {
            Ok(f) => f,
            Err(e) => {
                self.toasts
                    .err(format!("this run's form does not read: {e}"));
                return;
            }
        };
        self.opts.load(&form.settings);
        let files = form
            .files
            .iter()
            .map(|r| {
                let a = g.artifacts.iter().find(|a| a.id == r.id);
                Picked {
                    key: self.next_key(),
                    name: r.name.clone(),
                    size: a.map_or(0, |a| a.size),
                    kind: a.map_or_else(String::new, |a| kind_of_mime(&a.mime, &r.name)),
                    kept: Some(r.clone()),
                }
            })
            .collect();
        self.files.set(files);
        self.view.set(LabView::Form);
    }
}
