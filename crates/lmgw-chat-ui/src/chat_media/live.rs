//! A running media call (design §8.1): the stage line with the elapsed time,
//! the latest preview frame with its step, and Cancel — at once where a
//! cancel just drops the job or stops the wait, behind an inline confirm
//! where it restarts the image model.

use leptos::prelude::*;
use lmgw_api_types::chat_media::{CancelKind, MediaCancelled, MediaPhase, ToolProgressFrame};
use serde_json::json;

use super::{secs, stage_words, use_media};

/// What a running call's `progress` frames said so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaLive {
    /// The latest frame, its preview taken out (kept in `preview`).
    pub last: ToolProgressFrame,
    /// The stages it went through, in words, each once in a row: a queue
    /// place or a wait that changes rewrites its own entry.
    pub stages: Vec<String>,
    /// The latest preview (base64 PNG): a frame without one keeps it.
    pub preview: Option<String>,
    /// When the latest frame came (page clock, ms): its `elapsed_ms` plus
    /// the time since is the call's time now.
    pub at: f64,
}

impl MediaLive {
    pub fn apply(&mut self, f: &ToolProgressFrame, now: f64) {
        let words = stage_words(f);
        let same_phase = self.last.phase.is_some() && self.last.phase == f.phase;
        match self.stages.last_mut() {
            Some(last) if same_phase || *last == words => *last = words,
            _ => self.stages.push(words),
        }
        if f.preview.is_some() {
            self.preview = f.preview.clone();
        } else if f.phase != Some(MediaPhase::Generating) {
            // A stage before the drawing (a resubmitted job loading again)
            // has no picture yet.
            self.preview = None;
        }
        self.last = ToolProgressFrame {
            preview: None,
            ..f.clone()
        };
        self.at = now;
    }

    /// The call's time at page time `now`.
    pub fn elapsed(&self, now: f64) -> u64 {
        self.last.elapsed_ms + (now - self.at).max(0.0) as u64
    }

    /// `Step 3 / 8`, with the pass once a later one began.
    pub fn step(&self) -> Option<String> {
        let (step, total) = (self.last.step?, self.last.total_steps?);
        Some(match self.last.pass {
            Some(p) if p > 1 => format!("Step {step} / {total}, pass {p}"),
            _ => format!("Step {step} / {total}"),
        })
    }
}

/// Where the cancel stands.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CancelUi {
    Idle,
    /// A restart waits for its confirm.
    Confirm,
    Sent,
}

/// A page-unique number for an element's id.
fn next_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// What a cancel does now, said on the button's tooltip.
fn cancel_title(kind: Option<CancelKind>, model: &str) -> String {
    match kind {
        None => "Stop waiting: nothing is being drawn yet".to_string(),
        Some(CancelKind::Native) => {
            format!("Stop the picture: {model} drops it and stays loaded")
        }
        Some(CancelKind::Abandon) => format!(
            "Stop waiting: another request is using {model}, so its picture finishes in the \
             background and is thrown away"
        ),
        Some(CancelKind::Restart) => format!(
            "Cancel restarts {model}: it cannot stop mid-picture, so lmgw stops its container"
        ),
    }
}

/// The live pane of a running call: `live` its frames, `call_id` what the
/// cancel route names it by, `aspect` the picture's asked size, `what` the
/// tool (said until the first frame comes). `compact` is a card action's
/// slot under its picture: a thumbnail-sized preview.
#[component]
pub(super) fn LivePane(
    #[prop(into)] live: Signal<Option<MediaLive>>,
    #[prop(into)] call_id: Signal<Option<String>>,
    #[prop(into)] aspect: Signal<Option<(u32, u32)>>,
    #[prop(into)] what: Signal<String>,
    #[prop(optional)] compact: bool,
) -> impl IntoView {
    let http = lmgw_ui_kit::http::use_http();
    let ctx = use_media();
    let mounted = js_sys::Date::now();
    let now = RwSignal::new(mounted);
    // The clock the elapsed time reads; a stopped clock reads as instant at
    // this rate (`agent::CANCEL_POLL`, the gateway's poll).
    if let Ok(h) = set_interval_with_handle(
        move || now.set(js_sys::Date::now()),
        std::time::Duration::from_millis(250),
    ) {
        on_cleanup(move || h.clear());
    }
    let cancel = RwSignal::new(CancelUi::Idle);
    // The restart confirm takes the focus on its safe choice, Keep drawing;
    // leaving it (Keep drawing, Escape) gives it back to Cancel.
    let keep_ref: NodeRef<leptos::html::Button> = NodeRef::new();
    let cancel_ref: NodeRef<leptos::html::Button> = NodeRef::new();
    let refocus = StoredValue::new(false);
    Effect::new(move |_| {
        if let Some(b) = keep_ref.get() {
            let _ = b.focus();
        }
    });
    Effect::new(move |_| {
        if let Some(b) = cancel_ref.get() {
            if refocus.get_value() {
                refocus.set_value(false);
                let _ = b.focus();
            }
        }
    });
    let keep_drawing = move || {
        refocus.set_value(true);
        cancel.set(CancelUi::Idle);
    };
    let said_id = format!("media-confirm-{}", next_id());
    let model = move || {
        live.with(|l| {
            l.as_ref()
                .filter(|l| l.last.phase != Some(MediaPhase::Unloading))
                .and_then(|l| l.last.model.clone())
        })
        .unwrap_or_else(|| "the image model".to_string())
    };
    let kind = move || live.with(|l| l.as_ref().and_then(|l| l.last.cancel));
    let send = move || {
        let (Some(tid), Some(cid)) = (ctx.thread.get_untracked(), call_id.get_untracked()) else {
            return;
        };
        cancel.set(CancelUi::Sent);
        ctx.scope.spawn(async move {
            let cid_enc = js_sys::encode_uri_component(&cid)
                .as_string()
                .unwrap_or(cid);
            let url = format!("/chat/api/threads/{tid}/media/{cid_enc}/cancel");
            if let Err(e) = http.post::<MediaCancelled, _>(url, &json!({})).await {
                match &e {
                    lmgw_ui_kit::http::Error::Api(a) if a.code == "not_running" => {}
                    _ => ctx.toasts.err(format!("cancelling failed: {e}")),
                }
                cancel.set(CancelUi::Idle);
            }
        });
    };
    let on_cancel = move |_| match kind() {
        Some(CancelKind::Restart) => cancel.set(CancelUi::Confirm),
        _ => send(),
    };
    let stages = move || {
        live.with(|l| match l {
            Some(l) => l.stages.join(" · "),
            None => {
                let w = what.get();
                if w.starts_with("audio__") {
                    "Waiting for the answer".to_string()
                } else {
                    "Waiting for the picture".to_string()
                }
            }
        })
    };
    let elapsed = move || {
        let t = now.get();
        live.with(|l| match l {
            Some(l) => l.elapsed(t),
            None => (t - mounted).max(0.0) as u64,
        })
    };
    let box_style = move || {
        let (w, h) = aspect.get().unwrap_or((1, 1));
        super::frame_style(w, h)
    };
    let preview = move || live.with(|l| l.as_ref().and_then(|l| l.preview.clone()));
    let is_image = move || !what.with(|w| w.starts_with("audio__"));
    let download = Memo::new(move |_| {
        live.with(|l| {
            l.as_ref()
                .and_then(|l| lmgw_ui_kit::download::Dl::of_frame(&l.last))
        })
    });
    let downloading = Memo::new(move |_| download.with(Option::is_some));
    view! {
        <div class="media-live" class:compact=compact data-call-id=move || call_id.get()>
            <Show when=is_image>
                <div class="media-frame media-live-frame" style=box_style>
                    {move || match preview() {
                        Some(b64) => {
                            view! {
                                <img
                                    class="media-preview"
                                    alt="preview of the picture being drawn"
                                    src=format!("data:image/png;base64,{b64}")
                                />
                            }
                                .into_any()
                        }
                        None => view! { <span class="media-spinner" aria-hidden="true"></span> }.into_any(),
                    }}
                </div>
            </Show>
            <div class="media-live-text">
                <div class="media-stage">
                    <Show when=move || !is_image()>
                        <span class="media-spinner sm" aria-hidden="true"></span>
                    </Show>
                    <span class="media-stage-words" title=move || download.get().map(|d| d.title())>
                        {stages}
                    </span>
                </div>
                // The download before the model can load (machine-check
                // design §5): its bar, indeterminate while its size is not
                // known.
                <Show when=move || downloading.get()>
                    <lmgw_ui_kit::download::DlBar dl=Signal::derive(move || {
                        download.get().unwrap_or_default()
                    })/>
                </Show>
                <div class="media-step dim mono-sm">
                    {move || live.with(|l| l.as_ref().and_then(MediaLive::step))
                        .map(|s| format!("{s} · "))}
                    {move || secs(elapsed())}
                </div>
                <div class="media-live-actions">
                    {move || match cancel.get() {
                        CancelUi::Idle => {
                            view! {
                                <button
                                    class="btn sm"
                                    node_ref=cancel_ref
                                    disabled=move || call_id.with(Option::is_none)
                                    title=move || cancel_title(kind(), &model())
                                    on:click=on_cancel
                                >
                                    "Cancel"
                                </button>
                            }
                                .into_any()
                        }
                        CancelUi::Confirm => {
                            let said_id = said_id.clone();
                            view! {
                                <div
                                    class="media-confirm"
                                    role="alertdialog"
                                    aria-label="Restart the image model to cancel?"
                                    aria-describedby=said_id.clone()
                                    on:keydown=move |ev| {
                                        if ev.key() == "Escape" {
                                            ev.prevent_default();
                                            ev.stop_propagation();
                                            keep_drawing();
                                        }
                                    }
                                >
                                    <span id=said_id.clone()>
                                        {move || format!(
                                            "Cancel restarts {}: it cannot stop mid-picture, so lmgw \
                                             stops its container, and the next picture loads it again.",
                                            model(),
                                        )}
                                    </span>
                                    <span class="media-confirm-btns">
                                        <button class="btn danger sm" on:click=move |_| send()>
                                            "Restart and cancel"
                                        </button>
                                        <button
                                            class="btn ghost sm"
                                            node_ref=keep_ref
                                            on:click=move |_| keep_drawing()
                                        >
                                            "Keep drawing"
                                        </button>
                                    </span>
                                </div>
                            }
                                .into_any()
                        }
                        CancelUi::Sent => {
                            view! { <span class="dim mini-note">"Cancelling…"</span> }.into_any()
                        }
                    }}
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(phase: MediaPhase, step: Option<u32>, preview: Option<&str>) -> ToolProgressFrame {
        ToolProgressFrame {
            call_id: "call_1".into(),
            phase: Some(phase),
            model: Some("image/z".into()),
            pass: step.map(|_| 1),
            step,
            total_steps: step.map(|_| 8),
            elapsed_ms: 100,
            preview: preview.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn stages_are_said_once_in_a_row_and_the_preview_stays_until_replaced() {
        let mut l = MediaLive::default();
        let mut unload = frame(MediaPhase::Unloading, None, None);
        unload.model = Some("chat/q".into());
        l.apply(&unload, 1.0);
        l.apply(&frame(MediaPhase::Loading, None, None), 2.0);
        l.apply(&frame(MediaPhase::Generating, Some(1), Some("AAA")), 3.0);
        l.apply(&frame(MediaPhase::Generating, Some(2), None), 4.0);
        assert_eq!(
            l.stages,
            ["Unloading chat/q", "Loading image/z", "Generating"]
        );
        assert_eq!(l.preview.as_deref(), Some("AAA"));
        assert_eq!(l.step().as_deref(), Some("Step 2 / 8"));
        assert_eq!(l.last.preview, None);
        let mut second = frame(MediaPhase::Generating, Some(1), Some("BBB"));
        second.pass = Some(2);
        l.apply(&second, 5.0);
        assert_eq!(l.step().as_deref(), Some("Step 1 / 8, pass 2"));
        assert_eq!(l.preview.as_deref(), Some("BBB"));
        assert_eq!(l.elapsed(1005.0), 1100);
    }

    #[test]
    fn a_changing_queue_place_rewrites_its_own_stage() {
        let mut l = MediaLive::default();
        let mut q = frame(MediaPhase::Queued, None, None);
        q.queue_position = Some(2);
        l.apply(&q, 1.0);
        q.queue_position = Some(1);
        l.apply(&q, 2.0);
        assert_eq!(l.stages, ["Queued (1 ahead)"]);
    }
}
