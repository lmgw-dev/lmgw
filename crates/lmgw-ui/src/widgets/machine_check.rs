//! The machine check (machine-check design §1, §2, §4.4): whether this
//! computer can run local models, in plain words, and the fix.
//!
//! One component, the same wherever it shows (Settings → GPU's "This
//! computer" card, the Overview's card, the card above Chat's empty state
//! and the Chat's `gpu_setup` card, [`surfaces`]): the headline, the items
//! with a mark, the sentence and a "Details" fold each, the plan block, and
//! "Check again". The surface puts it in its own card.
//!
//! - **The state** ([`state`]) is the app's: the shown check, its reads and
//!   the setup's run, so every surface shows the same run and a page left
//!   while the setup runs shows it again when it comes back (D160).
//! - **The plan block** ([`plan`]): the plan's sentences, the commands
//!   behind "Show the commands", and "Set it up" in the app's window when
//!   the plan is the helper's; the commands to type when it is
//!   instructions; §2.7's sentence in a browser, which gets no button that
//!   could not work. "In the app's window" is the dashboard's one test for
//!   shell-only controls: `__TAURI_INTERNALS__` is there
//!   ([`bridge::in_app`]).
//! - **"Set it up"** calls the shell's `machine_apply` with the shown plan's
//!   hash ([`bridge`]); the shell checks again, asks in a dialog of its own,
//!   then the computer asks for the password. The progress view
//!   ([`progress`]) follows the command's events, and its answer's sentence
//!   is what the person reads, its detail and output in folds; the check it
//!   carries replaces the shown one.
//! - **The settings switch** ([`fit`]): the model settings item's action,
//!   confirmed under the item, no password. A surface that shows the
//!   settings (Settings) is told when it wrote them, to read them again.
//! - **On a surface with little room** (`compact`: the Overview's card, the
//!   card above Chat's empty state, the fix under a `gpu_setup` card) the
//!   items that are no problem fold into "What else was checked", so the
//!   problem and its fix come first (D186).
//!
//! "Try the graphics card" (§1.7) is not built yet: core has no probe for
//! it yet.

use leptos::prelude::*;
use lmgw_api_types::machine::{ItemState, MachineItemId, MachineVerdict};

/// The shell's `machine_apply`, its channel, and the in-app test.
mod bridge;
/// The settings-fit confirm.
mod fit;
/// Whether local models are involved, from the admin plane's rows.
mod involved;
/// The items.
mod items;
/// When the check reaches out beyond Settings, and the one "Not now".
mod nudge;
/// The plan block.
mod plan;
/// The progress view.
mod progress;
/// The app's check and run.
mod state;
/// The Overview's card, the Chat's banner and block, the Chat's slot.
mod surfaces;

#[cfg(test)]
mod tests;

pub(crate) use bridge::in_app;
pub use state::provide_machine_state;
pub(crate) use state::{use_machine_state, MachineState};
pub(crate) use surfaces::card_heading;
pub use surfaces::{chat_slot, MachineCard, MachineInCard};

use items::{DetailFold, ItemRow, Mark, MarkView};
use plan::PlanBlock;
use progress::{line_class, tone, ProgressView};

/// Where lmgw runs, as the component's own sentences name it (review M3):
/// "this computer" in the app's window; a browser may be on another one.
pub(crate) fn this_computer(app: bool) -> &'static str {
    if app {
        "this computer"
    } else {
        "the computer lmgw runs on"
    }
}

/// What the fixes under the items share: the switch being confirmed, its
/// write being out, and what a switch that found nothing to do said.
#[derive(Clone, Copy)]
pub(crate) struct FitSlot {
    pub offer: RwSignal<Option<fit::Offer>>,
    /// The switch's write is out: "Check again" and Cancel wait (review
    /// L2), so its answer is not lost.
    pub busy: RwSignal<bool>,
    /// "The model settings already are the ones for AMD cards.": a plain
    /// note under the item once the confirm closed (review M5).
    pub note: RwSignal<Option<String>>,
}

/// The whole check, its plan and the fix.
#[component]
pub fn MachineCheck(
    /// Ask for a new run when it is shown (§2.6: opening the Settings card,
    /// the Overview checklist mounting); without, the last result.
    #[prop(optional)]
    fresh: bool,
    /// A surface with little room (the Overview's card, the card above
    /// Chat's empty state, the fix under a `gpu_setup` card): the items
    /// that are no problem fold into one "What else was checked", so the
    /// problem and its fix come first (D186).
    #[prop(optional)]
    compact: bool,
    /// The settings switch wrote the settings: a surface that shows them
    /// reads them again (review H1).
    #[prop(optional, into)]
    settings_written: Option<Callback<()>>,
) -> impl IntoView {
    let st = use_machine_state();
    let app = in_app();
    let slot = FitSlot {
        offer: RwSignal::new(None),
        busy: RwSignal::new(false),
        note: RwSignal::new(None),
    };
    st.mount(fresh);

    let again = move |_| {
        if slot.busy.get_untracked() || !st.again() {
            return;
        }
        slot.offer.set(None);
        slot.note.set(None);
    };
    let set_up = Callback::new(move |hash: String| st.set_up(hash));
    let waits = Signal::derive(move || st.busy.get() || st.checking.get());

    let head = move || {
        let failed = st.error.with(Option::is_some);
        st.check.with(|c| {
            let (mark, headline, when, utc) = match c {
                Some(c) => {
                    let t = crate::fmt::log_time(&c.checked_at);
                    (
                        Mark::verdict(c.verdict),
                        c.headline.clone(),
                        format!("Checked {} at {}", t.day_label, t.time),
                        t.utc,
                    )
                }
                // Both reads failed: no check to show (review L6).
                None if failed => (
                    Mark::Problem,
                    format!("lmgw couldn't check {}.", this_computer(app)),
                    String::new(),
                    String::new(),
                ),
                None => (
                    Mark::NotChecked,
                    format!("Checking {}…", this_computer(app)),
                    String::new(),
                    String::new(),
                ),
            };
            // "Checking again…" only beside a check: without one, the
            // headline says it.
            let shown = c.is_some();
            view! {
                <MarkView mark=mark big=true/>
                <div class="mc-headline">
                    <div class="mc-verdict">{headline}</div>
                    <div class="mc-when" title=utc>
                        {move || {
                            if shown && st.checking.get() { "Checking again…".to_string() } else { when.clone() }
                        }}
                    </div>
                </div>
            }
        })
    };
    // On a compact surface the items that are no problem fold (D186).
    let ids = move |problems: bool| {
        st.check.with(|c| {
            c.as_ref()
                .map(|c| {
                    c.items
                        .iter()
                        .filter(|i| !compact || (i.state == ItemState::Problem) == problems)
                        .map(|i| i.id)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    };
    let item_ids = move || ids(true);
    let folded_ids = Memo::new(move |_| if compact { ids(false) } else { Vec::new() });
    let outcome = move || {
        st.run.with(|r| {
            let p = r.as_ref()?;
            if let Some(e) = &p.error {
                return Some(
                    view! {
                        <div class="mc-note err" role="alert">
                            {format!("The setup didn't start: {e}")}
                        </div>
                    }
                    .into_any(),
                );
            }
            let a = p.result.as_ref()?;
            let output = a.output.clone();
            let n = output.len();
            Some(
                view! {
                    <div class=format!("mc-note {}", tone(a.outcome)) role="status">
                        <div>{a.sentence.clone()}</div>
                        <DetailFold lines=a.detail.clone()/>
                        {(n > 0)
                            .then(|| {
                                view! {
                                    <details class="mc-fold">
                                        <summary>{format!("What the setup printed ({n} lines)")}</summary>
                                        <div class="mc-out">
                                            {output
                                                .into_iter()
                                                .map(|l| view! { <div class=line_class(None)>{l}</div> })
                                                .collect_view()}
                                        </div>
                                    </details>
                                }
                            })}
                    </div>
                }
                .into_any(),
            )
        })
    };
    let plan_block = move || {
        if st.busy.get() {
            return None;
        }
        let shown_terminal = st.terminal.get();
        st.check.with(|c| {
            c.as_ref().and_then(|c| c.plan.clone()).map(|p| {
                view! {
                    <PlanBlock
                        plan=p
                        in_app=app
                        terminal=shown_terminal
                        busy=waits
                        on_set_up=set_up
                        compact=compact
                    />
                }
            })
        })
    };

    // The setup's progress, its answer and the plan: under the item the
    // plan is for.
    let fix = move || {
        view! {
            // Shown while the command is out, and after it when a step
            // began; an end that ran nothing is its answer's sentence alone.
            <Show when=move || {
                st.run.with(|r| r.as_ref().is_some_and(|p| p.running() || p.any_began()))
            }>
                <ProgressView progress=st.run/>
            </Show>
            {outcome}
            {plan_block}
        }
    };
    // §1.2's item is where the fix goes, when it is in the list shown.
    let has_home = Memo::new(move |_| {
        st.check.with(|c| {
            c.as_ref().is_some_and(|c| {
                c.items.iter().any(|i| {
                    i.id == MachineItemId::CardForModels
                        && (!compact || i.state == ItemState::Problem)
                })
            })
        })
    });
    let cloud_only = Memo::new(move |_| {
        st.check.with(|c| {
            c.as_ref()
                .is_some_and(|c| c.verdict == MachineVerdict::CloudOnly)
        })
    });

    view! {
        <div class="machine-check">
            <div class="mc-head">
                {head}
                <button
                    class="btn sm"
                    disabled=move || st.checking.get() || st.busy.get() || slot.busy.get()
                    on:click=again
                >
                    "Check again"
                </button>
            </div>
            {move || {
                st.error
                    .get()
                    .map(|e| {
                        // Beside a check shown, it says which read failed;
                        // alone, the headline says it and this is why.
                        let shown = st.check.with(Option::is_some);
                        let text = if shown {
                            format!("lmgw couldn't check {} again: {e}", this_computer(app))
                        } else {
                            e
                        };
                        view! { <div class="mc-note err" role="alert">{text}</div> }
                    })
            }}
            <ul class="mc-items">
                <For
                    each=item_ids
                    key=|id| items::key(*id)
                    children=move |id| {
                        // The plan is §1.2's (the toolkit): it shows under
                        // that item, next to what it fixes.
                        let here = (id == MachineItemId::CardForModels).then(|| ViewFn::from(fix));
                        view! {
                            <ItemRow
                                id=id
                                st=st
                                fit_slot=slot
                                cloud_only=cloud_only
                                settings_written=settings_written
                                fix=here
                            />
                        }
                    }
                />
            </ul>
            // A check without that item still shows its plan, under the list.
            {move || (!has_home.get()).then(|| view! { <div class="mc-fix">{fix()}</div> })}
            <Show when=move || folded_ids.with(|f| !f.is_empty())>
                <details class="mc-fold mc-more">
                    <summary>
                        {move || format!("What else was checked ({})", folded_ids.with(Vec::len))}
                    </summary>
                    <ul class="mc-items">
                        <For
                            each=move || folded_ids.get()
                            key=|id| items::key(*id)
                            children=move |id| {
                                view! {
                                    <ItemRow
                                        id=id
                                        st=st
                                        fit_slot=slot
                                        cloud_only=cloud_only
                                        settings_written=settings_written
                                        fix=None
                                    />
                                }
                            }
                        />
                    </ul>
                </details>
            </Show>
        </div>
    }
}
