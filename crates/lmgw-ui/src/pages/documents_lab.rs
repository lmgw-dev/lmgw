//! Documents lab (document-conversion design WP12): the document-conversion
//! API tried from the dashboard, as the Audio and Image labs do for theirs.
//! Drop files, set the options, convert, look at what came back.
//!
//! The page posts to `/documents-lab/api/*`, which builds the multipart a
//! client would send from the kept form and files and hands it to the real
//! handler of `/v1/documents/convert` (sync, or as a job) or of the
//! docling-compatible `/v1/convert/file` in process: the options judged by
//! the route, its warnings and notes, the tool pool and the usage row are
//! the route's own. An async run's job is followed on the public
//! `/v1/documents/jobs/{id}` (the session is the owner's, who sees every
//! job), and cancelled there.
//!
//! Every run is kept in the lab store (labs-store design §5), on the frame,
//! history and presets the other labs share: the form and the files as a
//! group first, then its run, each document's text a file the store keeps.
//! A run goes back into the form with its files as kept files ("Open in
//! form"), so a re-run with changed options uploads nothing again.
//!
//! The rendered markdown comes from untrusted documents: it goes through the
//! Chat's sanitising renderer ([`rendered`]). A long result is shown a page
//! at a time ([`pager`]).

use std::collections::BTreeMap;

use leptos::prelude::*;
use lmgw_api_types::documents::convert::ConvertWarning;
use lmgw_api_types::documents::DocCaps;
use lmgw_api_types::documents_lab::{DocumentsLabForm, CONVERT_ENDPOINT, DOCLING_ENDPOINT};
use lmgw_api_types::lab_store::{LabGroupView, LabKind, LabPresetBody, LabPresetKind};
use serde_json::json;

use super::lab_frame::{Check, LabFrame, LabView, TaskGroup, TaskMenu, TaskOpt};
use super::lab_history::{ExportMenu, HistoryList, LabHistory};
use super::lab_presets::PresetsMenu;
use crate::fmt::fmt_bytes;
use crate::scope::Scope;
use crate::widgets::{Ico, Icon, Switch, Toasts};

/// A kept conversion's card.
mod card;
/// The form: the files and the options.
mod form;
/// An async run's job, live.
mod job;
/// The calls: make a group, run it, open a kept one in the form.
mod kept;
/// A file's structure as an outline.
mod outline;
/// A long text or structure a page at a time.
mod pager;
/// Markdown from a document, rendered inert.
mod rendered;
/// A kept run read file by file.
mod result;
/// What this computer converts, beside the form.
mod side;
/// One file's result: stats, warnings, its text in each form.
mod viewer;
/// Codes in plain words.
mod words;

use form::{head_of, sniff, FormView, Opts, Picked};

#[derive(Clone, Copy)]
struct Lab {
    files: RwSignal<Vec<Picked>>,
    /// The files picked in this window, by their row's key: the uploads.
    uploads: StoredValue<Vec<(u32, web_sys::File)>>,
    keys: StoredValue<u32>,
    opts: Opts,
    /// What this computer converts (`doc_tools`).
    caps: RwSignal<Option<DocCaps>>,
    caps_err: RwSignal<Option<String>>,
    /// The last run's notes on the request's options.
    notes: RwSignal<Vec<ConvertWarning>>,
    running: RwSignal<bool>,
    /// The group whose request this page has out.
    live: RwSignal<Option<i64>>,
    /// This page's async runs: group → job.
    jobs: RwSignal<BTreeMap<i64, String>>,
    history: LabHistory,
    view: RwSignal<LabView>,
    aborter: StoredValue<Option<web_sys::AbortController>>,
    expert: RwSignal<bool>,
    scope: Scope,
    toasts: Toasts,
}

impl Lab {
    fn next_key(self) -> u32 {
        self.keys
            .try_update_value(|k| {
                *k += 1;
                *k
            })
            .unwrap_or_default()
    }

    /// New files at the end of the list, each told by its first bytes.
    fn add_files(self, files: Vec<web_sys::File>) {
        for file in files {
            let key = self.next_key();
            let name = file.name();
            let size = file.size() as u64;
            self.files.update(|v| {
                v.push(Picked {
                    key,
                    name: name.clone(),
                    size,
                    kind: "…".into(),
                    kept: None,
                })
            });
            self.uploads.update_value(|u| u.push((key, file.clone())));
            self.scope.spawn(async move {
                let head = head_of(&file).await;
                let kind = sniff(&head, &name);
                self.files.update(|v| {
                    if let Some(p) = v.iter_mut().find(|p| p.key == key) {
                        p.kind = kind;
                    }
                });
            });
        }
    }

    fn remove_file(self, i: usize) {
        let mut gone = None;
        self.files.update(|v| {
            if i < v.len() {
                gone = Some(v.remove(i).key);
            }
        });
        if let Some(k) = gone {
            self.uploads
                .update_value(|u| u.retain(|(key, _)| *key != k));
        }
    }

    /// Stop the request out: an async run's job is cancelled on its route,
    /// a sync one's fetch is aborted (the route stops its tools).
    fn stop(self) {
        let job = self
            .live
            .get_untracked()
            .and_then(|id| self.jobs.with_untracked(|j| j.get(&id).cloned()));
        if let Some(job) = job {
            let href = format!("{}/{job}", lmgw_api_types::documents_lab::JOBS_ENDPOINT);
            self.scope.spawn(async move {
                let _ = gloo_net::http::Request::delete(&href).send().await;
            });
            return;
        }
        if let Some(a) = self.aborter.get_value() {
            a.abort();
        }
    }

    fn check(self) -> Check {
        let n = self.files.with(Vec::len);
        if n == 0 {
            return Check::Needs("pick one or more files".into());
        }
        let settings = match self.opts.settings() {
            Ok(s) => s,
            Err(e) => return Check::Invalid(e),
        };
        if let Err(e) = settings.fields() {
            return Check::Invalid(e);
        }
        let bytes: u64 = self.files.with(|f| f.iter().map(|p| p.size).sum());
        let mut line = format!(
            "{} · {} · {}",
            settings.endpoint(),
            if n == 1 {
                "1 file".to_string()
            } else {
                format!("{n} files")
            },
            fmt_bytes(bytes)
        );
        if settings.options.run_async {
            line.push_str(" · answers at once with a job");
        }
        Check::Ready(line)
    }

    fn run(self) {
        if self.running.get_untracked() {
            return;
        }
        let settings = match untrack(|| self.opts.settings()) {
            Ok(s) => s,
            Err(e) => {
                self.toasts.err(e);
                return;
            }
        };
        let picked = self.files.get_untracked();
        let files: Vec<(Picked, Option<web_sys::File>)> = self.uploads.with_value(|u| {
            picked
                .into_iter()
                .map(|p| {
                    let f = u.iter().find(|(k, _)| *k == p.key).map(|(_, f)| f.clone());
                    (p, f)
                })
                .collect()
        });
        let data = match kept::build_upload(&settings, &files) {
            Ok(d) => d,
            Err(e) => {
                self.toasts.err(e);
                return;
            }
        };
        self.send(data);
    }

    /// Keep `data` as a group and run it.
    fn send(self, data: web_sys::FormData) {
        self.view.set(LabView::Results);
        self.running.set(true);
        let ctrl = web_sys::AbortController::new().ok();
        let signal = ctrl.as_ref().map(|c| c.signal());
        self.aborter.set_value(ctrl);
        self.scope.spawn(async move {
            match kept::make_group(data, signal.as_ref()).await {
                Ok(g) => {
                    let id = g.id;
                    self.live.set(Some(id));
                    self.history.prepend(g);
                    kept::run_group(self, id, signal.as_ref()).await;
                    self.take_notes(id);
                }
                Err(e) if kept::is_abort(&e) => {}
                Err(e) => self.toasts.err(e),
            }
            self.history.refresh_usage();
            self.live.set(None);
            self.running.set(false);
            self.aborter.set_value(None);
        });
    }

    /// The landed run's notes on the options, for the form.
    fn take_notes(self, id: i64) {
        let notes = self.history.groups.with_untracked(|v| {
            v.iter().find(|g| g.id == id).and_then(|g| {
                let run = g.runs.first()?;
                let docling = run.endpoint == DOCLING_ENDPOINT;
                Some(result::read(run, docling, true).notes)
            })
        });
        if let Some(n) = notes {
            self.notes.set(n);
        }
    }

    /// The same files and options as a new group.
    fn run_again(self, g: &LabGroupView) {
        if self.running.get_untracked() {
            self.toasts.err("a run is out: wait for it, or stop it");
            return;
        }
        let Ok(form) = serde_json::from_value::<DocumentsLabForm>(g.form.clone()) else {
            self.toasts.err("this run's form does not read");
            return;
        };
        let Ok(data) = web_sys::FormData::new() else {
            return;
        };
        let Ok(json) = serde_json::to_string(&form) else {
            return;
        };
        let _ = data.append_with_str("form", &json);
        self.send(data);
    }

    fn load_caps(self) {
        self.scope.spawn(async move {
            match crate::api::post::<DocCaps, _>("/api/op/doc_tools", &json!({})).await {
                Ok(c) => {
                    self.caps.set(Some(c));
                    self.caps_err.set(None);
                }
                Err(e) => self.caps_err.set(Some(format!(
                    "What this computer converts did not load: {e}"
                ))),
            }
        });
    }
}

#[component]
pub fn DocumentsLab() -> impl IntoView {
    let scope = Scope::new();
    let lab = Lab {
        files: RwSignal::new(Vec::new()),
        uploads: StoredValue::new(Vec::new()),
        keys: StoredValue::new(0),
        opts: Opts::new(),
        caps: RwSignal::new(None),
        caps_err: RwSignal::new(None),
        notes: RwSignal::new(Vec::new()),
        running: RwSignal::new(false),
        live: RwSignal::new(None),
        jobs: RwSignal::new(BTreeMap::new()),
        history: LabHistory::new(LabKind::Documents, scope),
        view: RwSignal::new(LabView::Form),
        aborter: StoredValue::new(None),
        expert: crate::prefs::persisted_bool("documents-lab.expert", false),
        toasts: crate::widgets::use_toasts(),
        scope,
    };
    lab.load_caps();
    on_cleanup(move || {
        lab.aborter
            .try_with_value(|a| a.as_ref().map(web_sys::AbortController::abort));
    });

    let check = Memo::new(move |_| lab.check());
    let routes = Signal::derive(move || {
        vec![TaskGroup {
            label: "",
            opts: vec![
                TaskOpt {
                    id: "native".into(),
                    label: "Convert".into(),
                    tag: CONVERT_ENDPOINT.into(),
                    native: false,
                    off: None,
                },
                TaskOpt {
                    id: "docling".into(),
                    label: "As docling-serve".into(),
                    tag: DOCLING_ENDPOINT.into(),
                    native: false,
                    off: None,
                },
            ],
        }]
    });
    let card = Callback::new(move |id: i64| view! { <card::DocCard lab=lab id=id/> }.into_any());
    let preset_now = Callback::new(move |()| {
        LabPresetBody::DocumentOptions(Box::new(
            untrack(|| lab.opts.settings()).unwrap_or_default(),
        ))
    });
    let preset_load = Callback::new(move |b: LabPresetBody| {
        if let LabPresetBody::DocumentOptions(s) = b {
            lab.opts.load(&s);
        }
    });
    // What the history can be filtered by here: the two routes.
    let filter_models = Signal::derive(|| {
        vec![
            lmgw_api_types::documents_lab::NATIVE_MODEL.to_string(),
            lmgw_api_types::documents_lab::DOCLING_MODEL.to_string(),
        ]
    });

    view! {
        <LabFrame
            title="Documents lab"
            glyph=Ico::File
            empty="Every conversion lands here, newest first, and stays on the server until you delete it: one from its card, or all with Clear."
            persist="documents-lab.side"
            side_label="This computer"
            side_badge=Signal::derive(String::new)
            side=move || view! { <side::SidePanel lab=lab/> }
            model=move || {
                view! {
                    <span class="mp-src">
                        {move || {
                            if lab.opts.docling.get() {
                                "files in, docling's answer out"
                            } else {
                                "files in, text out"
                            }
                        }}
                    </span>
                }
            }
            head=move || {
                view! {
                    <TaskMenu
                        value=Signal::derive(move || {
                            if lab.opts.docling.get() { "docling" } else { "native" }.to_string()
                        })
                        groups=routes
                        on_pick=Callback::new(move |t: String| lab.opts.docling.set(t == "docling"))
                    />
                }
            }
            actions=move || {
                view! {
                    <label class="doclab-expert" title="Show each block's box in the structure and the answer as the gateway kept it. The request field names beside each setting show either way.">
                        <Switch value=lab.expert label="Expert view"/>
                        <span>"Expert view"</span>
                    </label>
                    <PresetsMenu
                        lab=LabKind::Documents
                        kind=LabPresetKind::DocumentOptions
                        title="Option presets"
                        current=preset_now
                        on_load=preset_load
                    />
                    <button
                        class="icon-btn lab-reload"
                        title="Read again what this computer converts"
                        aria-label="Reload"
                        on:click=move |_| lab.load_caps()
                    >
                        <Icon ico=Ico::Refresh/>
                    </button>
                }
            }
            form=move || {
                view! {
                    <div class="doclab-form" class:expert=move || lab.expert.get()>
                        <FormView lab=lab/>
                    </div>
                }
            }
            results=move || view! { <HistoryList history=lab.history card=card/> }
            results_n=Signal::derive(move || lab.history.total.get() as usize)
            check=check
            run_label=Signal::derive(|| "Convert".to_string())
            running=lab.running
            on_run=Callback::new(move |()| lab.run())
            on_stop=Callback::new(move |()| lab.stop())
            on_clear=Callback::new(move |()| lab.history.clear())
            view=lab.view
            kept=Signal::derive(move || lab.history.usage.get())
            results_tools=move || view! { <ExportMenu history=lab.history models=filter_models/> }
        />
    }
}
