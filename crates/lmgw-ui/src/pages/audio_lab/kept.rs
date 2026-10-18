//! The Audio lab's form and its kept groups (labs-store design §5): the
//! page's whole form as a group stores it and back ("Open in form", an
//! upload fetched back from the store as the file it was), the speech
//! settings a preset keeps, the route a request asks, and the group made
//! before each request and read again once its answer ends, until it is no
//! longer in flight.

use gloo_net::http::Request;
use leptos::prelude::*;
use lmgw_api_types::audio_lab::{
    AsrForm, AudioAsk, AudioLabForm, AudioRoute, AudioSettings, PcmFormat, TaskForm,
};
use lmgw_api_types::lab_store::{LabArtifactRole, LabGroupView, LabPresetBody};

use super::super::lab_frame::LabView;
use super::{Dispatch, Lab};
use crate::api;

impl Lab {
    /// The route the form as it is now asks.
    pub(super) fn route(self) -> AudioRoute {
        let f = self.form;
        if self.use_generic() {
            if f.task_stream.get_untracked() {
                AudioRoute::TaskStream
            } else {
                AudioRoute::TaskRun
            }
        } else if self.task() == "tts" {
            AudioRoute::Speech
        } else if f.asr_details.get_untracked() {
            AudioRoute::TranscriptionDetails
        } else {
            AudioRoute::Transcription
        }
    }

    /// The speech panel's settings as a preset keeps them.
    pub(super) fn settings(self) -> AudioSettings {
        let f = self.form;
        AudioSettings {
            speech: untrack(|| self.speech_form()),
            pcm: PcmFormat {
                rate: f.pcm_rate.get_untracked(),
                channels: f.pcm_channels.get_untracked(),
                bits: f.pcm_bits.get_untracked(),
            },
        }
    }

    /// The page's whole form, as a group stores it.
    pub(super) fn lab_form(self) -> AudioLabForm {
        let f = self.form;
        AudioLabForm {
            task: self.task_sel.get_untracked(),
            generic: self.generic.get_untracked(),
            settings: self.settings(),
            asr: AsrForm {
                source: f.asr_source.get_untracked(),
                path: f.asr_path.get_untracked(),
                file_name: f.asr_file_name.get_untracked(),
                stream: f.asr_stream.get_untracked(),
                details: f.asr_details.get_untracked(),
            },
            tasks: TaskForm {
                values: f.task_values.get_untracked(),
                flags: f.task_flags.get_untracked(),
                json: f.task_json.get_untracked(),
                stream: f.task_stream.get_untracked(),
            },
            timeline_rate: f.timeline_rate.get_untracked(),
        }
    }

    /// The model `model` picked, when the list has it, without the model's
    /// own task put over a task loaded with it.
    fn pick_model(self, model: &str, keep_task: bool) {
        let model = model.trim();
        let listed = self
            .models
            .with_untracked(|v| v.iter().any(|m| m.alias == model));
        if !listed || model == self.alias.get_untracked() {
            return;
        }
        if keep_task {
            self.loaded.set_value(Some(model.to_string()));
        }
        self.alias.set(model.to_string());
    }

    /// A preset's speech settings into the speech panel.
    pub(super) fn load_settings(self, s: &AudioSettings) {
        let f = self.form;
        let sp = &s.speech;
        if !sp.model.trim().is_empty() {
            self.pick_model(&sp.model, false);
        }
        f.input.set(sp.input.clone());
        f.voice.set(sp.voice.clone());
        f.voice_ref.set(sp.voice_ref.clone());
        f.reference_text.set(sp.reference_text.clone());
        f.speed.set(sp.speed.clone());
        f.language.set(sp.language.clone());
        f.instructions.set(sp.instructions.clone());
        f.seed.set(sp.seed.clone());
        f.max_tokens.set(sp.max_tokens.clone());
        f.response_format.set(sp.response_format.clone());
        f.stream.set(sp.stream);
        f.options.set(sp.options.clone());
        f.pcm_rate.set(s.pcm.rate.clone());
        f.pcm_channels.set(s.pcm.channels.clone());
        f.pcm_bits.set(s.pcm.bits.clone());
    }

    /// "Open in form": the group's model, task and every panel, its upload
    /// fetched back from the store into the file slot.
    pub(super) fn open_in_form(self, g: &LabGroupView) {
        let form = match serde_json::from_value::<AudioLabForm>(g.form.clone()) {
            Ok(f) => f,
            Err(e) => {
                self.toasts
                    .err(format!("this run's form does not read: {e}"));
                return;
            }
        };
        if let Some(c) = g.plan.first() {
            self.pick_model(&c.model, true);
        }
        let f = self.form;
        if !form.task.is_empty() {
            self.task_sel.set(form.task.clone());
        }
        self.generic.set(form.generic);
        let mut settings = form.settings.clone();
        // The model is the plan's, picked above.
        settings.speech.model = String::new();
        self.load_settings(&settings);
        f.asr_source.set(form.asr.source.clone());
        f.asr_path.set(form.asr.path.clone());
        f.asr_stream.set(form.asr.stream);
        f.asr_details.set(form.asr.details);
        f.task_values.set(form.tasks.values.clone());
        f.task_flags.set(form.tasks.flags.clone());
        f.task_json.set(form.tasks.json.clone());
        f.task_stream.set(form.tasks.stream);
        if !form.timeline_rate.is_empty() {
            f.timeline_rate.set(form.timeline_rate.clone());
        }
        f.asr_file.set_value(None);
        f.asr_file_name.set(String::new());
        // This group's upload is the one the slot waits for now: an earlier
        // Open in form still fetching lands nowhere, and neither does this
        // one once a file is picked by hand.
        let ticket = f.asr_reads.next();
        if let Some(a) = g
            .artifacts
            .iter()
            .find(|a| a.role == LabArtifactRole::Input)
            .cloned()
        {
            self.scope.spawn(async move {
                let got = fetch_file(&a.href, &a.name, &a.mime).await;
                if !ticket.is_some_and(|t| f.asr_reads.is(t)) {
                    return;
                }
                match got {
                    Ok(file) => {
                        f.asr_file_name.set(file.name());
                        f.asr_file.set_value(Some(file));
                    }
                    Err(e) => self
                        .toasts
                        .err(format!("the kept upload did not load: {e}")),
                }
            });
        }
        self.view.set(LabView::Form);
    }

    /// The group a request is kept in, made before it is sent.
    pub(super) async fn make_group(self, route: AudioRoute) -> Result<LabGroupView, String> {
        let ask = AudioAsk {
            model: self.alias.get_untracked(),
            route,
            form: self.lab_form(),
        };
        api::post::<LabGroupView, _>("/audio-lab/api/groups", &ask)
            .await
            .map_err(|e| e.to_string())
    }

    /// The group a request is kept in, made and put in front of the history,
    /// its card the live one; `None` (after saying why, with the page back
    /// to idle) when it could not be made, and nothing is sent.
    pub(super) async fn start_group(self, route: AudioRoute) -> Option<i64> {
        match self.make_group(route).await {
            Ok(g) => {
                let id = g.id;
                self.live.set(Some(id));
                self.history.prepend(g);
                Some(id)
            }
            Err(e) => {
                self.toasts
                    .err(format!("the run could not be kept, nothing was sent: {e}"));
                self.settle();
                self.running.set(false);
                self.aborter.set_value(None);
                None
            }
        }
    }

    /// The group `id` once its answer ended, read until it is no longer in
    /// flight (a stopped answer is recorded once the gateway sees the page
    /// go). A kept run takes the live card's place, and so does the stopped
    /// card of a Stop that kept nothing. Any other end without a kept run
    /// (the store failed, the lab refused the hand-off, the connection
    /// broke) keeps the live card, the answer still playable in it, and the
    /// group's note says it was not kept, with why when the page knows.
    pub(super) async fn land(self, id: i64, stopped: bool) {
        let got = self.history.settle(id).await;
        self.history.refresh_usage();
        let why = || {
            let e = self.error.get_untracked();
            if e.is_empty() {
                "the gateway could not record it; its log says why".to_string()
            } else {
                e
            }
        };
        match got {
            Ok(Some(g)) if !g.runs.is_empty() || stopped => self.settle(),
            Ok(Some(_)) => self.history.note(id, format!("not kept: {}", why())),
            Ok(None) => {
                // Deleted meanwhile (a Clear): its card is gone, the live
                // one above the history stays.
                self.error
                    .set("not kept: its group was deleted while the answer came".into());
            }
            Err(e) => {
                self.history.note(id, format!("not kept: {}", why()));
                self.toasts.err(format!("the kept run did not load: {e}"));
            }
        }
    }
}

/// `href`'s bytes as a `File` named `name`.
async fn fetch_file(href: &str, name: &str, mime: &str) -> Result<web_sys::File, String> {
    let resp = Request::get(href).send().await.map_err(|e| e.to_string())?;
    // A refusal as every call reads it: a `401 session_required` raises the
    // login gate.
    let resp = api::checked(resp).await.map_err(|e| e.to_string())?;
    let bytes = resp.binary().await.map_err(|e| e.to_string())?;
    let arr = js_sys::Uint8Array::from(bytes.as_slice());
    let parts = js_sys::Array::new();
    parts.push(&arr);
    let opts = web_sys::FilePropertyBag::new();
    opts.set_type(mime);
    web_sys::File::new_with_u8_array_sequence_and_options(&parts, name, &opts)
        .map_err(|_| "the browser would not make a file of it".to_string())
}

/// The speech settings a new preset keeps.
pub(super) fn preset_now(lab: Lab) -> LabPresetBody {
    LabPresetBody::AudioSettings(Box::new(lab.settings()))
}

/// A preset's body into the form.
pub(super) fn preset_load(lab: Lab, b: LabPresetBody) {
    if let LabPresetBody::AudioSettings(s) = b {
        lab.load_settings(&s);
    }
}

/// Send the request of group `group` (`?group=<id>` on its route).
pub(super) async fn send(
    dispatch: Dispatch,
    group: i64,
    signal: Option<&web_sys::AbortSignal>,
) -> Result<gloo_net::http::Response, String> {
    let req = match dispatch {
        Dispatch::Json { url, body } => Request::post(&with_group(&url, group))
            .abort_signal(signal)
            .json(&body),
        Dispatch::Form { url, form } => Request::post(&with_group(&url, group))
            .abort_signal(signal)
            .body(form),
    };
    req.map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())
}

/// `url` with `group=<id>` added to its query.
pub(super) fn with_group(url: &str, id: i64) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}group={id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_group_joins_the_query() {
        assert_eq!(
            with_group("/audio-lab/api/speech", 4),
            "/audio-lab/api/speech?group=4"
        );
        assert_eq!(
            with_group("/audio-lab/api/transcriptions?details=1", 4),
            "/audio-lab/api/transcriptions?details=1&group=4"
        );
    }
}
