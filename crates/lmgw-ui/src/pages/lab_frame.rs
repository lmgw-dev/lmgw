//! The layout the Audio and Image labs share (UX plan Phase 5 task 3).
//!
//! One head line — the model, the task, the model's state — then the form
//! column beside the run history, with the lab's reference panel folded to a
//! rail on the right below a 1600px pane. The form column scrolls on its own
//! and ends in a pinned action bar: the run button and the line that says
//! what it will call, or why it cannot yet — so the button is on screen at
//! any window height, and a request override that does not parse says so
//! there rather than inside a folded preview. The results column is every
//! run of the session, newest first, with the count and a Clear; the run in
//! flight leads it. Below a 760px body the two columns become one, switched
//! by a "Form | Results (N)" seg.

use leptos::html;
use leptos::prelude::*;

use crate::widgets::{popover, ConfirmButton, Popover, Side, SplitPane};

/// The main pane's floor beside the reference panel: a 1600px pane less the
/// panel's 360px ceiling. Below it the panel folds to its rail.
const SIDE_FOLDS_BELOW: u32 = 1240;

/// Whether the form can be sent, and if not, why — the action bar's line.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Check {
    /// Sendable; the line says what will be called.
    Ready(String),
    /// Something is still to be filled in or picked.
    Needs(String),
    /// Something that is filled in does not parse.
    Invalid(String),
}

/// Which column a one-column (narrow) lab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LabView {
    Form,
    Results,
}

/// One choice in the task menu.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct TaskOpt {
    pub id: String,
    pub label: String,
    /// The mono tag after the label: a task id, a route.
    pub tag: String,
    /// The selected model's own task (a green dot).
    pub native: bool,
    /// Why it cannot be picked. It stays listed, greyed, with the reason —
    /// hiding it would leave the question unanswered.
    pub off: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TaskGroup {
    /// An eyebrow above the group; empty for none.
    pub label: &'static str,
    pub opts: Vec<TaskOpt>,
}

#[derive(Clone, Debug, PartialEq)]
enum MenuLine {
    Head(&'static str),
    Opt(TaskOpt),
}

/// A request field's wire name after its label, breakable after each `.` and
/// `_`: in a narrow form column `sample_params.guidance.txt_cfg` wraps at its
/// dots rather than mid-word or past the column's edge.
#[component]
pub(super) fn Fname(name: &'static str) -> impl IntoView {
    let parts: Vec<&'static str> = name.split_inclusive(['.', '_']).collect();
    view! {
        <i class="fname">
            {parts.into_iter().map(|p| view! { {p} <wbr/> }).collect_view()}
        </i>
    }
}

/// A Select-shaped button over a grouped list of tasks, some of them greyed
/// with the reason they are not served.
#[component]
pub(super) fn TaskMenu(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] groups: Signal<Vec<TaskGroup>>,
    on_pick: Callback<String>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let btn: NodeRef<html::Button> = NodeRef::new();
    let list: NodeRef<html::Div> = NodeRef::new();
    let active = RwSignal::new(None::<usize>);
    let was_open = StoredValue::new(false);
    let lines = Memo::new(move |_| {
        groups
            .get()
            .into_iter()
            .flat_map(|g| {
                let head = (!g.label.is_empty()).then_some(MenuLine::Head(g.label));
                head.into_iter()
                    .chain(g.opts.into_iter().map(MenuLine::Opt))
            })
            .collect::<Vec<_>>()
    });
    let pickable = move |i: usize| {
        lines.with_untracked(|v| matches!(v.get(i), Some(MenuLine::Opt(o)) if o.off.is_none()))
    };
    Effect::new(move |_| {
        if open.get() {
            let cur = value.get_untracked();
            active.set(lines.with_untracked(|v| {
                v.iter()
                    .position(|l| matches!(l, MenuLine::Opt(o) if o.id == cur))
            }));
        }
    });
    let step = move |from: Option<usize>, down: bool| {
        let n = lines.with_untracked(Vec::len);
        let mut i = from;
        for _ in 0..n {
            let next = match (i, down) {
                (None, true) => 0,
                (None, false) => n - 1,
                (Some(k), true) => (k + 1) % n,
                (Some(k), false) => (k + n - 1) % n,
            };
            if pickable(next) {
                return Some(next);
            }
            i = Some(next);
        }
        None
    };
    let focus_btn = move || {
        if let Some(b) = btn.get_untracked() {
            let _ = b.focus();
        }
    };
    let choose = move |i: usize| {
        let Some(MenuLine::Opt(o)) = lines.with_untracked(|v| v.get(i).cloned()) else {
            return;
        };
        if o.off.is_some() {
            return;
        }
        open.set(false);
        focus_btn();
        on_pick.run(o.id);
    };
    let set_active = move |i: Option<usize>| {
        active.set(i);
        if let (Some(i), Some(l)) = (i, list.get_untracked()) {
            popover::reveal_child(&l, i);
        }
    };
    let on_key = move |ev: web_sys::KeyboardEvent| {
        let cur = active.get_untracked();
        match ev.key().as_str() {
            "ArrowDown" => set_active(step(cur, true)),
            "ArrowUp" => set_active(step(cur, false)),
            "Home" => set_active(step(None, true)),
            "End" => set_active(step(None, false)),
            "Enter" | " " => {
                if let Some(i) = cur {
                    choose(i);
                }
            }
            "Escape" => {
                ev.stop_propagation();
                open.set(false);
                focus_btn();
            }
            "Tab" => {
                open.set(false);
                focus_btn();
                return;
            }
            _ => return,
        }
        ev.prevent_default();
    };
    let chosen = move || {
        let v = value.get();
        lines.with(|l| {
            l.iter().find_map(|x| match x {
                MenuLine::Opt(o) if o.id == v => Some(o.clone()),
                _ => None,
            })
        })
    };

    view! {
        <div class="select task-menu">
            <button
                type="button"
                class="input select-btn task-btn"
                node_ref=btn
                aria-haspopup="listbox"
                aria-expanded=move || open.get().to_string()
                on:pointerdown=move |_| was_open.set_value(open.get_untracked())
                on:click=move |_| {
                    let reopen = !was_open.get_value();
                    was_open.set_value(false);
                    open.set(reopen && !open.get_untracked());
                }
                on:keydown=move |ev| {
                    if !open.get_untracked() && matches!(ev.key().as_str(), "ArrowDown" | "ArrowUp") {
                        ev.prevent_default();
                        open.set(true);
                    }
                }
            >
                {move || match chosen() {
                    Some(o) => {
                        view! {
                            <span class="task-val">
                                <i class="task-dot" class:native=o.native></i>
                                {o.label}
                                <i class="tid">{o.tag}</i>
                            </span>
                        }
                            .into_any()
                    }
                    None => view! { <span class="task-val dim">{value.get()}</span> }.into_any(),
                }}
                <span class="select-arrow">"▾"</span>
            </button>
            <Popover open=open anchor=btn class="menu-pop task-pop">
                <div
                    class="pop-list menu-list task-list"
                    role="listbox"
                    tabindex="-1"
                    node_ref=list
                    data-autofocus
                    on:keydown=on_key
                >
                    {move || {
                        lines
                            .get()
                            .into_iter()
                            .enumerate()
                            .map(|(i, line)| match line {
                                MenuLine::Head(label) => {
                                    view! { <div class="task-head">{label}</div> }.into_any()
                                }
                                MenuLine::Opt(o) => {
                                    let off = o.off.is_some();
                                    let sel = move || value.get() == o.id;
                                    view! {
                                        <div
                                            class="menu-item task-opt"
                                            class:active=move || active.get() == Some(i)
                                            class:disabled=off
                                            role="option"
                                            aria-selected=move || sel().to_string()
                                            aria-disabled=off.then_some("true")
                                            on:pointermove=move |_| {
                                                if !off && active.get_untracked() != Some(i) {
                                                    active.set(Some(i));
                                                }
                                            }
                                            on:click=move |_| choose(i)
                                        >
                                            <span class="task-line">
                                                <i class="task-dot" class:native=o.native></i>
                                                <span class="task-label">{o.label.clone()}</span>
                                                <i class="tid">{o.tag.clone()}</i>
                                                {o
                                                    .native
                                                    .then(|| {
                                                        view! { <span class="task-note">"this model's task"</span> }
                                                    })}
                                            </span>
                                            {o
                                                .off
                                                .clone()
                                                .map(|why| view! { <span class="task-why">{why}</span> })}
                                        </div>
                                    }
                                        .into_any()
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </Popover>
        </div>
    }
}

/// The shared lab layout. Everything lab-specific comes in as a slot or a
/// signal; the columns, the action bar, the results head and the narrow seg
/// are the frame's.
#[component]
pub(super) fn LabFrame(
    /// `lmgw.ui.side.<persist>` for the reference panel.
    persist: &'static str,
    #[prop(into)] side_label: TextProp,
    /// The reference panel's pill ("2" clips); empty for none.
    #[prop(into)]
    side_badge: Signal<String>,
    /// The reference panel.
    #[prop(into)]
    side: ViewFn,
    /// The head line's controls.
    #[prop(into)]
    head: ViewFn,
    /// The form column: notices, cards, the request preview.
    #[prop(into)]
    form: ViewFn,
    /// The run in flight and the history, newest first.
    #[prop(into)]
    results: ViewFn,
    /// Finished runs held (the Clear's count).
    #[prop(into)]
    results_n: Signal<usize>,
    #[prop(into)] check: Signal<Check>,
    #[prop(into)] run_label: Signal<String>,
    #[prop(into)] running: Signal<bool>,
    on_run: Callback<()>,
    on_stop: Callback<()>,
    on_clear: Callback<()>,
    view: RwSignal<LabView>,
) -> impl IntoView {
    let shown_n = move || results_n.get() + usize::from(running.get());
    let note = move || match check.get() {
        Check::Ready(s) => ("action-note", s),
        Check::Needs(s) => ("action-note needs", s),
        Check::Invalid(s) => ("action-note bad", format!("⚠ {s}")),
    };
    let ready = move || matches!(check.get(), Check::Ready(_));
    let any_held = move || results_n.get() > 0;

    view! {
        <div class="lab-shell density-dense">
            <div class="lab-head">{head.run()}</div>
            <SplitPane
                side=Side::Right
                persist=persist
                label=side_label
                badge=side_badge
                auto_collapse_below=SIDE_FOLDS_BELOW
                side_view=side
            >
                <div class="lab-body">
                    <div
                        class="lab-cols"
                        data-view=move || match view.get() {
                            LabView::Form => "form",
                            LabView::Results => "results",
                        }
                    >
                        <div class="seg lab-seg">
                            <button
                                type="button"
                                class="seg-btn"
                                class:active=move || view.get() == LabView::Form
                                on:click=move |_| view.set(LabView::Form)
                            >
                                "Form"
                            </button>
                            <button
                                type="button"
                                class="seg-btn"
                                class:active=move || view.get() == LabView::Results
                                on:click=move |_| view.set(LabView::Results)
                            >
                                "Results"
                                <span class="count">{shown_n}</span>
                            </button>
                        </div>
                        <section class="lab-form">
                            <div class="lab-scroll">{form.run()}</div>
                            <div class="action-bar">
                                {move || {
                                    let (cls, text) = note();
                                    let title = text.clone();
                                    view! {
                                        <span class=cls title=title>
                                            {text}
                                        </span>
                                    }
                                }}
                                {move || {
                                    if running.get() {
                                        view! {
                                            <span class="chip live">
                                                <i class="dot"></i>
                                                "running…"
                                            </span>
                                            <button class="btn danger" on:click=move |_| on_stop.run(())>
                                                "Stop"
                                            </button>
                                        }
                                            .into_any()
                                    } else {
                                        view! {
                                            <button
                                                class="btn primary"
                                                disabled=move || !ready()
                                                on:click=move |_| on_run.run(())
                                            >
                                                {move || run_label.get()}
                                            </button>
                                        }
                                            .into_any()
                                    }
                                }}
                            </div>
                        </section>
                        <section class="lab-results">
                            <div class="results-head">
                                <span class="results-title">"Results"</span>
                                <span class="count">{shown_n}</span>
                                <span class="results-note dim">"newest first · this session only"</span>
                                <span class="spacer"></span>
                                <Show when=any_held>
                                    // The history is the only copy of a result
                                    // that was not downloaded: nothing on the
                                    // server keeps it.
                                    <ConfirmButton
                                        label="Clear"
                                        confirm=move || format!("Drop all {}?", results_n.get())
                                        class="btn ghost sm"
                                        title="Drop every finished result of this session"
                                        on_confirm=on_clear
                                    />
                                </Show>
                            </div>
                            <div class="results-list">
                                <Show when=move || shown_n() == 0>
                                    <div class="empty">
                                        "Every run of this session lands here, newest first. Nothing "
                                        "is kept on the server — download what you want to keep."
                                    </div>
                                </Show>
                                {results.run()}
                            </div>
                        </section>
                    </div>
                </div>
            </SplitPane>
        </div>
    }
}
