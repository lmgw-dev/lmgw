//! A picture's card actions (design §8.2, §8.3): Re-roll, Vary or Edit, and
//! Upscale run on `POST /chat/api/attachments/{id}/media` with no chat model
//! in the loop, streaming `action`, `progress`, `done` or `error`; Save
//! downloads the picture; Attach puts a copy in the composer.
//!
//! The picture an action makes is an attachment of the same message
//! (`source` `action`), so it joins the card's strip and is selected; the
//! model sees it only once attached.

use leptos::prelude::*;
use lmgw_api_types::chat_media::{
    MediaAction, MediaActionDone, MediaActionRequest, MediaActionStarted, ToolProgressFrame,
};
use lmgw_api_types::chat_threads::AttachmentMeta;
use serde_json::Value;

use super::live::MediaLive;
use super::{facts, take_files, use_media, MediaCtx};
use crate::chat_bytes::{attachment_path, Saver};
use crate::chat_stream::post_sse;

/// The inline form an action opens.
#[derive(Debug, Clone, PartialEq)]
enum Form {
    /// A variation (img2img) or an instruction edit: a prompt, and for a
    /// variation how far to move.
    Change { edit: bool },
    /// Upscale's confirm: it runs on a local model only.
    Upscale,
}

/// A card action that runs or ran, kept by the page ([`MediaCtx`]'s runs)
/// rather than by its card: a card that is left (another thread opened)
/// and shown again finds it, with its live pane, its Cancel and its buttons
/// held while it runs. One per family: a new action replaces the last.
#[derive(Clone)]
pub(crate) struct Run {
    /// Which run it is, page-wide.
    seq: u64,
    /// The picture it runs on.
    source: i64,
    /// The family's pictures when it started (its card finds it by them).
    family: Vec<i64>,
    busy: bool,
    /// The action's id (`act-<boot>-<n>`), once its stream said it.
    call_id: Option<String>,
    live: Option<MediaLive>,
    /// The tool the action is said as, for the live pane before its first
    /// frame.
    what: String,
    /// The action's refusal or failure, in the gateway's words.
    notice: Option<String>,
    /// Where its picture goes: the message's files and the card's pick of
    /// the card that shows it now (a card shown again takes it over).
    files: RwSignal<Vec<AttachmentMeta>>,
    picked: RwSignal<Option<i64>>,
    /// Closes the stream, should the page go.
    ctrl: Option<web_sys::AbortController>,
}

impl Run {
    /// Close its stream, if it is still read.
    pub fn abort(&self) {
        if let Some(c) = &self.ctrl {
            c.abort();
        }
    }
}

/// One family's card-side state, and its window onto the page's run.
/// `Copy`: the signals live with the card.
#[derive(Clone, Copy)]
pub(super) struct Runner {
    ctx: MediaCtx,
    files: RwSignal<Vec<AttachmentMeta>>,
    picked: RwSignal<Option<i64>>,
    /// The family's pictures (the root's and every action picture's ids).
    family: Memo<Vec<i64>>,
    /// The newest run of the family, by its `seq`.
    mine: Memo<Option<u64>>,
    /// A notice of the card's own (a strength that is no number).
    local_notice: RwSignal<Option<String>>,
    form: RwSignal<Option<Form>>,
    /// The form's fields.
    prompt: RwSignal<String>,
    strength: RwSignal<String>,
    /// The gateway said the model takes the other kind of change
    /// (`wrong_action`): the card offers that one.
    flipped: RwSignal<bool>,
}

impl Runner {
    pub fn new(
        files: RwSignal<Vec<AttachmentMeta>>,
        picked: RwSignal<Option<i64>>,
        family: Memo<Vec<i64>>,
    ) -> Self {
        let ctx = use_media();
        let runs = ctx.runs;
        let mine = Memo::new(move |_| {
            family.with(|fam| {
                runs.with(|v| {
                    v.iter()
                        .filter(|r| {
                            fam.contains(&r.source) || r.family.iter().any(|f| fam.contains(f))
                        })
                        .map(|r| r.seq)
                        .max()
                })
            })
        });
        // Shown again while its action runs: the action's picture comes to
        // this card's message and pick.
        if let Some(seq) = mine.get_untracked() {
            runs.update(|v| {
                if let Some(r) = v.iter_mut().find(|r| r.seq == seq && r.busy) {
                    r.files = files;
                    r.picked = picked;
                }
            });
        }
        Self {
            ctx,
            files,
            picked,
            family,
            mine,
            local_notice: RwSignal::new(None),
            form: RwSignal::new(None),
            prompt: RwSignal::new(String::new()),
            strength: RwSignal::new(String::new()),
            flipped: RwSignal::new(false),
        }
    }

    /// A field of the family's newest run.
    fn read<T>(self, f: impl Fn(&Run) -> T) -> Option<T> {
        let seq = self.mine.get()?;
        self.ctx
            .runs
            .with(|v| v.iter().find(|r| r.seq == seq).map(&f))
    }

    pub fn busy(self) -> bool {
        self.read(|r| r.busy).unwrap_or(false)
    }
    pub fn call_id(self) -> Option<String> {
        self.read(|r| r.call_id.clone()).flatten()
    }
    pub fn live(self) -> Option<MediaLive> {
        self.read(|r| r.live.clone()).flatten()
    }
    pub fn what(self) -> String {
        self.read(|r| r.what.clone()).unwrap_or_default()
    }
    fn notice(self) -> Option<String> {
        self.local_notice
            .get()
            .or_else(|| self.read(|r| r.notice.clone()).flatten())
    }

    /// Clear what the last action said (a form opens).
    fn clear_notice(self) {
        self.local_notice.set(None);
        if let Some(seq) = self.mine.get_untracked() {
            self.ctx.runs.update(|v| {
                if let Some(r) = v.iter_mut().find(|r| r.seq == seq && !r.busy) {
                    r.notice = None;
                }
            });
        }
    }

    /// Run `req` on picture `id` and follow its stream to its end. A second
    /// start while one runs is ignored (the buttons are held too).
    fn start(self, id: i64, req: MediaActionRequest) {
        if self.read_untracked_busy() {
            return;
        }
        let ctx = self.ctx;
        let runs = ctx.runs;
        let seq = runs.with_untracked(|v| v.iter().map(|r| r.seq).max().unwrap_or(0) + 1);
        let family = self.family.get_untracked();
        let ctrl = web_sys::AbortController::new().ok();
        let what = match req.action {
            MediaAction::Upscale => "image__upscale",
            MediaAction::Reroll => "image__generate",
            MediaAction::Vary | MediaAction::Edit => "image__edit",
        }
        .to_string();
        runs.update(|v| {
            // The family's last run is replaced (its notice with it).
            v.retain(|r| r.busy || !(family.contains(&r.source)));
            v.push(Run {
                seq,
                source: id,
                family,
                busy: true,
                call_id: None,
                live: None,
                what,
                notice: None,
                files: self.files,
                picked: self.picked,
                ctrl: ctrl.clone(),
            });
        });
        self.local_notice.set(None);
        self.form.set(None);
        let action = req.action;
        let body = serde_json::to_value(&req).unwrap_or(Value::Null);
        // The run's entry, changed; nothing once the page is gone.
        let edit = move |f: &mut dyn FnMut(&mut Run)| {
            runs.try_update(|v| {
                if let Some(r) = v.iter_mut().find(|r| r.seq == seq) {
                    f(r);
                }
            });
        };
        ctx.scope.spawn(async move {
            let url = format!("/chat/api/attachments/{id}/media");
            // Closing the stream would not stop the action (it runs in a
            // task of its own); the page closes it only when it goes.
            let Some(ctrl) = ctrl else {
                edit(&mut |r| r.busy = false);
                return;
            };
            let mut ended = false;
            let mut said: Option<(Option<String>, String)> = None;
            let signal = ctrl.signal();
            let res = post_sse(
                ctx.http,
                &url,
                &body,
                Some(&signal),
                |event, data| match event {
                    "action" => {
                        if let Ok(a) = serde_json::from_value::<MediaActionStarted>(data) {
                            edit(&mut |r| r.call_id = Some(a.call_id.clone()));
                        }
                    }
                    "progress" => {
                        if let Ok(f) = serde_json::from_value::<ToolProgressFrame>(data) {
                            let now = js_sys::Date::now();
                            edit(&mut |r| {
                                r.live.get_or_insert_with(MediaLive::default).apply(&f, now)
                            });
                        }
                    }
                    "done" => {
                        ended = true;
                        if let Ok(d) = serde_json::from_value::<MediaActionDone>(data) {
                            let to = runs.try_with_untracked(|v| {
                                v.iter().find(|r| r.seq == seq).map(|r| (r.files, r.picked))
                            });
                            if let Some(Some((files, picked))) = to {
                                take_files(files, std::slice::from_ref(&d.attachment));
                                // The newest is selected: the one just made.
                                picked.try_set(None);
                            }
                        }
                    }
                    "error" => {
                        ended = true;
                        let msg = data["message"]
                            .as_str()
                            .unwrap_or("the action failed")
                            .to_string();
                        said = Some((data["code"].as_str().map(str::to_string), msg));
                    }
                    _ => {}
                },
            )
            .await;
            let notice = match res {
                // Stopped (the page went): nothing to say. Told by the
                // signal, not by the transport's words.
                Err(_) if signal.aborted() => None,
                Err(r) => Some(self.refused(action, r.code.as_deref(), r.message)),
                Ok(()) => match said {
                    Some((code, msg)) => Some(self.refused(action, code.as_deref(), msg)),
                    None if !ended => Some(
                        "the action's stream ended without its picture; it may still arrive when \
                         the thread is read again"
                            .to_string(),
                    ),
                    None => None,
                },
            };
            edit(&mut |r| {
                r.busy = false;
                r.call_id = None;
                r.live = None;
                r.ctrl = None;
                r.notice = notice.clone();
            });
        });
    }

    /// Whether the family's newest run is running, untracked.
    fn read_untracked_busy(self) -> bool {
        let Some(seq) = self.mine.get_untracked() else {
            return false;
        };
        self.ctx
            .runs
            .with_untracked(|v| v.iter().any(|r| r.seq == seq && r.busy))
    }

    /// A refusal or failure in words for under the card, with what helps
    /// where the gateway's words alone do not.
    fn refused(self, action: MediaAction, code: Option<&str>, message: String) -> String {
        match code {
            Some("wrong_action") => {
                // The model takes the other kind of change: offer that one,
                // with the prompt as it was (on the card, if it is still
                // shown).
                self.flipped.try_update(|f| *f = !*f);
                let edit = matches!(action, MediaAction::Vary);
                self.form.try_set(Some(Form::Change { edit }));
                message
            }
            Some("cancelled") | Some("canceled") => "Cancelled.".to_string(),
            _ => message,
        }
    }
}

/// Whether a picture's model changes pictures by instruction (`edit`) rather
/// than by variation: a local row that serves edits does, a cloud alias does
/// (design §14 item 23); a model the catalog does not list is taken as a
/// variation row, and the gateway's `wrong_action` corrects it.
fn edits_by_instruction(entries: &[lmgw_ui_kit::catalog::CatalogEntry], model: &str) -> bool {
    match entries.iter().find(|m| m.id == model) {
        Some(m) if m.local => m.task.as_deref() == Some("image_edit"),
        Some(_) => true,
        None => false,
    }
}

/// The buttons under picture `a`, the open form, and the last notice.
/// `settled`: the reply is a stored message (the actions need one; while it
/// is written they are held).
#[component]
pub(super) fn ActionBar(
    a: AttachmentMeta,
    runner: Runner,
    #[prop(into)] settled: Signal<bool>,
) -> impl IntoView {
    let ctx = use_media();
    let saver = Saver::new();
    let f = facts(&a);
    let id = a.id;
    let is_upscale = f.upscaler.is_some() || f.tool.as_deref() == Some("image__upscale");
    let rerollable = f.prompt.is_some() && !is_upscale;
    let model = f.model.clone();
    let changeable = model.is_some() && !is_upscale;
    let catalog = lmgw_ui_kit::catalog::use_model_catalog();
    let edit = {
        let model = model.clone();
        Memo::new(move |_| {
            let by_catalog = model
                .as_deref()
                .is_some_and(|m| catalog.entries.with(|e| edits_by_instruction(e, m)));
            by_catalog != runner.flipped.get()
        })
    };
    // Held while an action runs, and until the reply is stored.
    let held = move || runner.busy() || !settled.get();
    let held_title = move |idle: &'static str| {
        if settled.get() {
            idle
        } else {
            "Once the reply is finished"
        }
    };
    let (prompt0, strength0) = (f.prompt.clone().unwrap_or_default(), f.strength);
    let open_change = move |_| {
        runner.prompt.set(prompt0.clone());
        runner
            .strength
            .set(strength0.map(|s| s.to_string()).unwrap_or_default());
        runner.clear_notice();
        runner.form.set(Some(Form::Change {
            edit: edit.get_untracked(),
        }));
    };
    let reroll = move |_| {
        runner.start(
            id,
            MediaActionRequest {
                action: MediaAction::Reroll,
                prompt: None,
                strength: None,
                upscaler: None,
                repeats: None,
            },
        )
    };
    let run_change = move |edit: bool| {
        let strength = runner.strength.get_untracked();
        let strength = match strength.trim() {
            "" => None,
            s => match s.parse::<f64>() {
                Ok(v) if (0.0..=1.0).contains(&v) => Some(v),
                _ => {
                    runner
                        .local_notice
                        .set(Some(format!("strength {s} is not a number from 0 to 1")));
                    return;
                }
            },
        };
        let prompt = runner.prompt.get_untracked();
        runner.start(
            id,
            MediaActionRequest {
                action: if edit {
                    MediaAction::Edit
                } else {
                    MediaAction::Vary
                },
                prompt: Some(prompt).filter(|p| !p.trim().is_empty()),
                strength: if edit { None } else { strength },
                upscaler: None,
                repeats: None,
            },
        );
    };
    let upscale = move |_| {
        runner.start(
            id,
            MediaActionRequest {
                action: MediaAction::Upscale,
                prompt: None,
                strength: None,
                upscaler: None,
                repeats: None,
            },
        )
    };
    let (save_path, save_name) = (attachment_path(id), a.name.clone());
    let attach_a = a.clone();
    let attaching = move || ctx.attaching.with(|v| v.contains(&id));
    view! {
        <div class="media-actions">
            {rerollable
                .then(|| {
                    view! {
                        <button
                            class="btn sm"
                            data-media-action="reroll"
                            disabled=held
                            title=move || held_title("The same prompt and settings with a new seed")
                            on:click=reroll
                        >
                            "Re-roll"
                        </button>
                    }
                })}
            {changeable
                .then(|| {
                    view! {
                        <button
                            class="btn sm"
                            data-media-action="change"
                            disabled=held
                            title=move || {
                                held_title(
                                    if edit.get() {
                                        "Change this picture by an instruction"
                                    } else {
                                        "Make a variation of this picture"
                                    },
                                )
                            }
                            on:click=open_change
                        >
                            {move || if edit.get() { "Edit" } else { "Vary" }}
                        </button>
                    }
                })}
            {(!is_upscale)
                .then(|| {
                    view! {
                        <button
                            class="btn sm"
                            data-media-action="upscale"
                            disabled=held
                            title=move || held_title("Make it larger with an upscaler, on a local image model")
                            on:click=move |_| {
                                runner.clear_notice();
                                runner.form.set(Some(Form::Upscale));
                            }
                        >
                            "Upscale"
                        </button>
                    }
                })}
            <span class="media-actions-gap"></span>
            <button
                class="btn sm"
                data-media-action="save"
                title="Save the picture"
                on:click=move |_| saver.save(save_path.clone(), save_name.clone())
            >
                "Save"
            </button>
            <button
                class="btn sm"
                data-media-action="attach"
                title="Add the picture to the next message, so the model sees it"
                disabled=attaching
                aria-busy=move || attaching().then_some("true")
                on:click=move |_| ctx.attach.run(attach_a.clone())
            >
                {move || if attaching() { "Attaching…" } else { "Attach" }}
            </button>
        </div>
        {move || match runner.form.get() {
            Some(Form::Change { edit }) => {
                view! {
                    <div class="media-form" data-media-form=if edit { "edit" } else { "vary" }>
                        <label class="field">
                            <span>{if edit { "What to change" } else { "Prompt" }}</span>
                            <textarea
                                class="input ta"
                                rows="2"
                                prop:value=move || runner.prompt.get()
                                on:input=move |ev| runner.prompt.set(event_target_value(&ev))
                            ></textarea>
                        </label>
                        {(!edit)
                            .then(|| {
                                view! {
                                    <label class="field media-strength">
                                        <span>"Strength (0 to 1)"</span>
                                        <input
                                            class="input mono"
                                            inputmode="decimal"
                                            placeholder="the model's default"
                                            prop:value=move || runner.strength.get()
                                            on:input=move |ev| runner.strength.set(event_target_value(&ev))
                                        />
                                    </label>
                                }
                            })}
                        <div class="media-form-btns">
                            <button class="btn primary sm" on:click=move |_| run_change(edit)>
                                {if edit { "Edit" } else { "Vary" }}
                            </button>
                            <button class="btn ghost sm" on:click=move |_| runner.form.set(None)>
                                "Cancel"
                            </button>
                        </div>
                    </div>
                }
                    .into_any()
            }
            Some(Form::Upscale) => {
                view! {
                    <div class="media-form" data-media-form="upscale">
                        <span class="media-form-note">
                            "Upscaling runs on a local image model only, never on a cloud fallback. "
                            "If none is loaded, one loads onto the GPU, which can unload the chat model."
                        </span>
                        <div class="media-form-btns">
                            <button class="btn primary sm" on:click=upscale>"Upscale"</button>
                            <button class="btn ghost sm" on:click=move |_| runner.form.set(None)>
                                "Cancel"
                            </button>
                        </div>
                    </div>
                }
                    .into_any()
            }
            None => ().into_any(),
        }}
        {move || {
            runner
                .notice()
                .map(|n| view! { <div class="media-notice" role="status">{n}</div> })
        }}
    }
}
