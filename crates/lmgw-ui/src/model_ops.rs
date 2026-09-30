//! Per-model container lifecycle controls (per-model-containers §8): the
//! pieces the Overview runtime table and the model editors both need —
//! Start/Stop/Restart/Apply buttons and the logs drawer — built on
//! `POST /api/op/container` (`ops::container`: target + model + action[,
//! override, tail]) and tracked per-(class, model) in [`crate::ops_state`] so
//! one model's cold start never disables another model's button.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{json, Value};

use crate::ops_state::{model_key, use_ops, OpsState};
use crate::widgets::{use_toasts, Modal, ModalSize, Toasts};

/// Engage or release the GPU hold (gpu-hold design §3.1, §6) through
/// `ops::hold_set` — the op the tray, the titlebar's GPU pill and Settings →
/// GPU → Hold all call. Engaging stops every local container, so it is never
/// part of a draft: it applies on the click. `busy` covers the round trip;
/// the live `vram` frame carries the new state back to every surface.
pub fn hold_set(toasts: Toasts, busy: RwSignal<bool>, active: bool) {
    if busy.get_untracked() {
        return;
    }
    busy.set(true);
    spawn_local(async move {
        let res =
            crate::api::post::<Value, _>("/api/op/hold_set", &json!({ "active": active })).await;
        busy.set(false);
        match res {
            Ok(v) => toasts.ok(v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(if active {
                    "GPU hold engaged"
                } else {
                    "GPU hold released"
                })
                .to_string()),
            Err(e) => toasts.err(e.to_string()),
        }
    });
}

/// Fire one `/api/op/container` call for a single model, claiming
/// `(class, model_id)` in [`OpsState`] for the duration. A click while the
/// same model is already under an op is a silent no-op — the button driving
/// it should already be `disabled` in that state, this is the re-entrancy
/// backstop.
fn fire(
    ops: OpsState,
    class: &'static str,
    model_id: String,
    action: &'static str,
    force: bool,
    tail: Option<i64>,
    then: impl FnOnce(crate::api::Result<Value>) + 'static,
) {
    let key = model_key(class, &model_id);
    if !ops.start(&key) {
        return;
    }
    spawn_local(async move {
        let mut body = json!({ "target": class, "model": model_id, "action": action });
        if force {
            body["override"] = json!(true);
        }
        if let Some(t) = tail {
            body["tail"] = json!(t);
        }
        let res = crate::api::post::<Value, _>("/api/op/container", &body).await;
        ops.finish(&key);
        then(res);
    });
}

fn toast_message(toasts: Toasts, res: crate::api::Result<Value>, fallback: &str) {
    match res {
        Ok(v) => {
            let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(true);
            let msg = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(fallback)
                .to_string();
            if ok {
                toasts.ok(msg);
            } else {
                // `model_apply`'s busy refusal: HTTP 200, `ok:false` — a
                // report, not a transport error, but still bad news.
                toasts.err(msg);
            }
        }
        Err(e) => toasts.err(e.to_string()),
    }
}

/// Start / Stop / Restart for one model — the Overview runtime table's
/// per-row actions. `running`: `Some(state)` (`starting|ready|stopping`) when
/// the runtime frame carries an entry for this model, `None` for an
/// enabled-but-not-running model.
#[component]
pub fn ModelRunButtons(
    class: &'static str,
    model_id: String,
    running: Option<String>,
) -> impl IntoView {
    let ops = use_ops();
    let toasts = use_toasts();
    // `StoredValue` (unlike a raw `String`) is `Copy`, so every closure below
    // that captures it stays `Copy`/`Fn` no matter how many times a reactive
    // block needs to rebuild it — a plain `String` capture would make the
    // closure `FnOnce` the moment it is used inside one of those.
    let mid = StoredValue::new(model_id);
    let busy = move || mid.with_value(|m| ops.busy(&model_key(class, m)));
    // Populated with the refusal message when a plain stop is denied
    // (in-flight requests) — offers the `override=true` retry the spec calls
    // for, rather than silently forcing it on the first click.
    let refused: RwSignal<Option<String>> = RwSignal::new(None);

    let start = move |_| {
        fire(
            ops,
            class,
            mid.get_value(),
            "start",
            false,
            None,
            move |res| {
                toast_message(toasts, res, "started");
            },
        );
    };
    let restart = move |_| {
        fire(
            ops,
            class,
            mid.get_value(),
            "restart",
            false,
            None,
            move |res| {
                toast_message(toasts, res, "restarted");
            },
        );
    };
    let do_stop = move |force: bool| {
        fire(
            ops,
            class,
            mid.get_value(),
            "stop",
            force,
            None,
            move |res| match res {
                Ok(v) => {
                    let msg = v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("stopped")
                        .to_string();
                    toasts.ok(msg);
                }
                Err(e) => {
                    // `model_stop`'s busy refusal is a transport error (HTTP
                    // 400) naming the `override=true` escape — offer it instead
                    // of just toasting the failure.
                    refused.set(Some(e.to_string()));
                }
            },
        );
    };
    let stop = move |_| do_stop(false);
    let force_stop = move || {
        refused.set(None);
        do_stop(true);
    };

    view! {
        <div class="row" style="flex-wrap:nowrap; justify-content:flex-end">
            {if running.is_some() {
                view! {
                    <button class="btn ghost" disabled=busy on:click=stop>
                        "Stop"
                    </button>
                    <button class="btn ghost" disabled=busy on:click=restart>
                        "Restart"
                    </button>
                }
                    .into_any()
            } else {
                view! {
                    <button class="btn ghost" disabled=busy on:click=start>
                        {move || if busy() { "Starting…" } else { "Start" }}
                    </button>
                }
                    .into_any()
            }}
        </div>
        {move || {
            refused
                .get()
                .map(|msg| {
                    view! {
                        <StopRefusedModal
                            message=msg
                            on_cancel=move || refused.set(None)
                            on_force=force_stop
                        />
                    }
                })
        }}
    }
}

/// Start, stop or restart one model from a row menu, where there is no button
/// to hold a busy state: the `(class, model)` claim in [`OpsState`] is what
/// keeps a second pick from firing a second command. A stop the gateway
/// refuses (requests in flight) goes to `on_refused` with its message, so the
/// page can offer the forced stop ([`StopRefusedModal`]) instead of toasting a
/// dead end.
pub fn container_action(
    ops: OpsState,
    toasts: Toasts,
    class: &'static str,
    model_id: String,
    action: &'static str,
    force: bool,
    on_refused: impl FnOnce(String) + 'static,
) {
    fire(
        ops,
        class,
        model_id,
        action,
        force,
        None,
        move |res| match (action, res) {
            ("stop", Err(e)) if !force => on_refused(e.to_string()),
            (_, res) => {
                let done = match action {
                    "start" => "started",
                    "restart" => "restarted",
                    "stop" => "stopped",
                    _ => "done",
                };
                toast_message(toasts, res, done);
            }
        },
    );
}

/// "Stop refused: requests are in flight" with the forced stop on offer —
/// what [`ModelRunButtons`] shows, for callers driving stops from elsewhere.
#[component]
pub fn StopRefusedModal(
    message: String,
    on_cancel: impl Fn() + Copy + Send + Sync + 'static,
    on_force: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let open = RwSignal::new(true);
    Effect::new(move |_| {
        if !open.get() {
            on_cancel();
        }
    });
    view! {
        <Modal open=open title="Stop refused">
            <p>{message.clone()}</p>
            <div class="row" style="justify-content:flex-end">
                <button class="btn ghost" on:click=move |_| open.set(false)>
                    "Keep it running"
                </button>
                <button
                    class="btn danger"
                    on:click=move |_| {
                        open.set(false);
                        on_force();
                    }
                >
                    "Force stop"
                </button>
            </div>
        </Modal>
    }
}

/// One model's Apply button — the model editors' replacement for the old
/// global apply bar (§8): visible only when the edited model is currently
/// running (apply on a stopped model is a no-op by design, §3.6), since Save
/// already persists the config and the backend already stops a running
/// container so the *next* request picks it up (`stop_for_apply`). Apply is
/// the deliberate "reload it warm right now" step on top of that.
/// Reactive on its own (reads the `runtime` frame directly) so a parent that
/// also renders a [`LogsButton`] next to it doesn't have to re-render both
/// on every registry change just to keep this one's visibility current —
/// that would reset the logs drawer's open state along with it.
#[component]
pub fn ModelApplyButton(class: &'static str, model_id: String) -> impl IntoView {
    let ops = use_ops();
    let toasts = use_toasts();
    let live = crate::live::use_live();
    // `StoredValue` is `Copy`, so `busy`/`running` stay `Fn` no matter how
    // many times `<Show>` has to rebuild its children (see `ModelRunButtons`).
    let mid = StoredValue::new(model_id);
    let busy = move || mid.with_value(|m| ops.busy(&model_key(class, m)));
    let running = move || {
        mid.with_value(|m| {
            live.runtime
                .get()
                .is_some_and(|rows| rows.iter().any(|r| r.class == class && &r.model_id == m))
        })
    };
    let apply = move |_| {
        fire(
            ops,
            class,
            mid.get_value(),
            "apply",
            false,
            None,
            move |res| {
                toast_message(toasts, res, "applied");
            },
        );
    };
    view! {
        <Show when=running>
            <button
                class="btn primary"
                disabled=busy
                title="Stop and restart this model's container now, with the current configuration"
                on:click=apply
            >
                {move || if busy() { "Applying…" } else { "Apply to container" }}
            </button>
        </Show>
    }
}

/// Logs button + drawer (a [`Modal`]) for one model — `action=logs`
/// (per-model-containers §3.6/§8), the only place a failed start is visible
/// with N containers. Shared by the Overview row and the model editors.
#[component]
pub fn LogsButton(class: &'static str, model_id: String) -> impl IntoView {
    let open = RwSignal::new(false);
    let disabled = model_id.trim().is_empty();
    view! {
        <button
            class="btn ghost"
            disabled=disabled
            title="podman logs for this model's container"
            on:click=move |_| open.set(true)
        >
            "Logs"
        </button>
        <Modal open=open title="Container logs" size=ModalSize::Wide fill=true>
            <LogsBody class=class model_id=model_id.clone() open=open/>
        </Modal>
    }
}

/// One logs drawer for a whole table: rows set `target` to `(class, model)`
/// from their menu, instead of each mounting a [`LogsButton`] and its modal.
#[component]
pub fn LogsModal(target: RwSignal<Option<(&'static str, String)>>) -> impl IntoView {
    let open = RwSignal::new(false);
    // Guarded both ways, as the model editors do: an unconditional set would
    // notify on an unchanged value and the two effects would chase each other.
    Effect::new(move |_| {
        let want = target.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && target.with_untracked(Option::is_some) {
            target.set(None);
        }
    });
    view! {
        <Modal open=open title="Container logs" size=ModalSize::Wide fill=true>
            {move || {
                target
                    .get()
                    .map(|(class, model_id)| view! { <LogsBody class=class model_id=model_id open=open/> })
            }}
        </Modal>
    }
}

#[component]
fn LogsBody(class: &'static str, model_id: String, open: RwSignal<bool>) -> impl IntoView {
    let mid = StoredValue::new(model_id.clone());
    let tail = RwSignal::new(String::from("60"));
    let reload = RwSignal::new(0u32);
    // Gated on `open`: the modal's children mount as soon as the button
    // does (`Modal` always renders `children()`, it just hides the dialog),
    // so without this every row would fire a logs fetch on page load.
    let logs = LocalResource::new(move || {
        let want = open.get();
        reload.get();
        let t: i64 = tail.get_untracked().trim().parse().unwrap_or(60);
        let model_id = model_id.clone();
        async move {
            if !want {
                return None;
            }
            Some(
                crate::api::post::<Value, _>(
                    "/api/op/container",
                    &json!({ "target": class, "model": model_id, "action": "logs", "tail": t }),
                )
                .await,
            )
        }
    });
    let refresh = move |_| reload.update(|n| *n += 1);
    // A tail is read from the bottom: each load lands on the newest line.
    let pre: NodeRef<leptos::html::Pre> = NodeRef::new();
    Effect::new(move |_| {
        if matches!(logs.get(), Some(Some(Ok(_)))) {
            request_animation_frame(move || {
                if let Some(p) = pre.get_untracked() {
                    p.set_scroll_top(p.scroll_height());
                }
            });
        }
    });
    view! {
        <div class="logs-bar">
            <span class="mono-sm">{move || mid.get_value()}</span>
            <span class="dim mono-sm logs-ctr">
                {move || {
                    logs.get()
                        .flatten()
                        .and_then(|r| r.ok())
                        .and_then(|v| v.get("container").and_then(Value::as_str).map(String::from))
                        .unwrap_or_default()
                }}
            </span>
            <span class="spacer"></span>
            <label class="row dim" style="gap:5px">
                "last"
                <input
                    class="input mono w-num"
                    title="How many lines from the end of the log to show"
                    data-untracked
                    prop:value=move || tail.get()
                    on:input=move |ev| tail.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            reload.update(|n| *n += 1);
                        }
                    }
                />
                "lines"
            </label>
            <button class="btn ghost sm" on:click=refresh>
                "Refresh"
            </button>
        </div>
        {move || match logs.get().flatten() {
            None => view! { <div class="dim">"Loading…"</div> }.into_any(),
            Some(Err(e)) => view! { <div class="notice err">{e.to_string()}</div> }.into_any(),
            Some(Ok(v)) => {
                let text = v
                    .get("logs")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("(no output)")
                    .to_string();
                view! { <pre class="preset fill-pane logs-pre" node_ref=pre>{text}</pre> }.into_any()
            }
        }}
    }
}

/// State chip + Logs + Apply for one model — the compact strip the model
/// editors (local_edit.rs, and the aux/audio forms in model_editors.rs) show
/// near their per-model container override fields. `model_id` empty (still
/// unsaved — create mode) hides the whole row: there is nothing to control
/// until the model exists.
#[component]
pub fn ContainerStatusRow(
    class: &'static str,
    #[prop(into)] model_id: Signal<String>,
) -> impl IntoView {
    let live = crate::live::use_live();
    let row = move || {
        let mid = model_id.get();
        if mid.is_empty() {
            return None;
        }
        live.runtime.get().and_then(|rows| {
            rows.into_iter()
                .find(|r| r.class == class && r.model_id == mid)
        })
    };
    let state = move || row().map(|r| r.state);
    view! {
        <div class="row" style="align-items:center; flex-wrap:nowrap">
            <span class="dim">"container:"</span>
            {move || {
                let (cls, label) = match state().as_deref() {
                    Some("ready") => ("chip ok", "ready"),
                    Some("starting") => ("chip live", "starting"),
                    Some("stopping") => ("chip live", "stopping"),
                    _ => ("chip off", "stopped"),
                };
                view! {
                    <span class=cls>
                        <span class="dot"></span>
                        {label}
                    </span>
                }
            }}
            // Ladder design §6: the rung this container runs, the gguf file
            // name a hover away; a row without a ladder has no `rung` at all,
            // so this renders nothing for it.
            {move || {
                row()
                    .and_then(|r| r.rung)
                    .map(|rung| {
                        view! {
                            <span class="type-badge" title=rung.gguf.clone()>
                                {format!("rung {}/{}", rung.rung, rung.of)}
                            </span>
                        }
                    })
            }}
            {move || {
                row()
                    .and_then(|r| r.climbing)
                    .map(|c| {
                        view! {
                            <span class="type-badge climbing-badge" title=c.reason.clone()>
                                {format!("climbing to {}/{}", c.to, c.of)}
                            </span>
                        }
                    })
            }}
            <div class="spacer" style="flex:1"></div>
            {move || {
                let mid = model_id.get();
                (!mid.is_empty()).then(|| view! { <LogsButton class=class model_id=mid/> })
            }}
            {move || {
                let mid = model_id.get();
                (!mid.is_empty()).then(|| view! { <ModelApplyButton class=class model_id=mid/> })
            }}
        </div>
    }
}
