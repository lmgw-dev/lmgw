//! The machine check's state for the whole app (machine-check design §4.4,
//! D160): the shown check, its reads and the setup's run, provided once in
//! `App` ([`provide_machine_state`]), so every `MachineCheck` shows the same
//! check and the same run: Settings → GPU's card, the Overview's card and
//! the Chat's (WP8b). It follows the live feed's `machine` frame, so a
//! check that changed while a page stays open is read again (D183).
//!
//! The run is the app's, not a page's: `machine_apply`'s call, its channel
//! callback and its answer outlive the page that pressed "Set it up". A page
//! left while the setup runs and opened again shows the run where it is (and
//! asks for no fresh check, whose plan would stand beside it), and the answer
//! of a run that ended meanwhile is there until the next run or "Check
//! again" (D155). The callback is unregistered at the end, before the next
//! run, and when the state itself goes (never from inside its own call).
//!
//! A host that provides no state (a test, another app) gets one per
//! component, cleaned up with it.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::machine::{
    MachineApplyOutcome, MachineCheck as Check, MachineCheckArgs, MachineSummary,
};

use super::bridge;
use super::progress::Progress;

/// The check, as `machine_check` answers it: the last result, or a new run.
async fn read(fresh: bool) -> Result<Check, String> {
    crate::api::post::<Check, _>("/api/op/machine_check", &MachineCheckArgs { fresh })
        .await
        .map_err(|e| e.to_string())
}

/// The shown check and the setup's run. `Copy`: pass it around like a
/// signal.
#[derive(Clone, Copy)]
pub struct MachineState {
    /// The check shown: the last read's, or the one a run or a settings
    /// switch answered with.
    pub(super) check: RwSignal<Option<Check>>,
    /// The last read's error.
    pub(super) error: RwSignal<Option<String>>,
    /// A read is out.
    pub(super) checking: RwSignal<bool>,
    /// The setup's run: none before the first "Set it up" and after "Check
    /// again".
    pub(super) run: RwSignal<Option<Progress>>,
    /// The commands to type stay open: the desktop had no password window
    /// (`no_agent`).
    pub(super) terminal: RwSignal<bool>,
    /// `machine_apply` is out. A memo, so an event that changes nothing of
    /// it does not rebuild what reads it (the plan block).
    pub(super) busy: Memo<bool>,
    listener: StoredValue<Option<bridge::Listener>, LocalStorage>,
}

/// Provide the app's one state, the one "Not now" of the surfaces that
/// reach out (`nudge`, WP8b) and whether local models are involved
/// (`involved`, D184), and follow the live feed's `machine` frame (D183).
/// Call once, in `App`, after the live bus.
pub fn provide_machine_state() {
    let st = MachineState::new();
    provide_context(st);
    provide_context(super::nudge::Nudge::new(st));
    provide_context(super::involved::Involvement::new());
    if let Some(live) = use_context::<crate::live::LiveBus>() {
        st.follow(live.machine);
    }
}

/// The app's state, or one of this component's own where none is provided.
pub(crate) fn use_machine_state() -> MachineState {
    use_context::<MachineState>().unwrap_or_else(MachineState::new)
}

impl MachineState {
    fn new() -> Self {
        let run = RwSignal::new(None::<Progress>);
        let listener = StoredValue::new_local(None::<bridge::Listener>);
        on_cleanup(move || {
            if let Some(l) = listener.try_update_value(Option::take).flatten() {
                l.stop();
            }
        });
        Self {
            check: RwSignal::new(None),
            error: RwSignal::new(None),
            checking: RwSignal::new(false),
            run,
            terminal: RwSignal::new(false),
            busy: Memo::new(move |_| run.with(|r| r.as_ref().is_some_and(Progress::running))),
            listener,
        }
    }

    /// A read's answer: the check, or the error beside the check shown.
    fn take(self, r: Result<Check, String>) {
        match r {
            Ok(c) => {
                self.check.try_set(Some(c));
                self.error.try_set(None);
            }
            Err(e) => {
                self.error.try_set(Some(e));
            }
        }
    }

    /// There is something to show (tracked): a check, or why there is none.
    pub(crate) fn shown_or_failed(self) -> bool {
        self.check.with(Option::is_some) || self.error.with(Option::is_some)
    }

    /// A check one of the component's own answers carried (the settings
    /// switch's write).
    pub(super) fn show(self, c: Check) {
        self.check.set(Some(c));
        self.error.set(None);
    }

    /// What a component asks for when it mounts (§2.6, D150): the last
    /// result when none is shown yet, then a new run when `fresh`. Nothing
    /// while a setup runs, whose answer carries the check, or while a read
    /// is out already (another surface's mount).
    pub(super) fn mount(self, fresh: bool) {
        if self.busy.get_untracked() || self.checking.get_untracked() {
            return;
        }
        let shown = self.check.with_untracked(Option::is_some);
        if shown && !fresh {
            return;
        }
        // "Set it up" waits while it is out (review L3): a plan the read
        // replaces must not be the one pressed.
        self.checking.set(true);
        spawn_local(async move {
            if !shown {
                self.take(read(false).await);
            }
            if fresh {
                self.take(read(true).await);
            }
            self.checking.try_set(false);
        });
    }

    /// The live feed's summary of the last result (D183): when it is not the
    /// shown check's, the last result is read again (`fresh: false`, no
    /// run), once per summary, so a dashboard left open sees a driver update
    /// that wants a restart or a failed start without a Chat turn. Nothing
    /// before any surface showed a check (its mount reads it), and nothing
    /// while a read or a setup is out: the effect looks again when it ends.
    fn follow(self, summary: ReadSignal<Option<MachineSummary>>) {
        let asked = StoredValue::new(None::<MachineSummary>);
        Effect::new(move |_| {
            let Some(s) = summary.get() else { return };
            if self.checking.get() || self.busy.get() {
                return;
            }
            let shown = self
                .check
                .with_untracked(|c| c.as_ref().map(MachineSummary::from));
            let stale = shown.is_some_and(|c| c != s);
            if !stale || asked.with_value(|a| a.as_ref() == Some(&s)) {
                return;
            }
            asked.set_value(Some(s));
            self.checking.set(true);
            spawn_local(async move {
                self.take(read(false).await);
                self.checking.try_set(false);
            });
        });
    }

    /// "Check again": a new run; the last setup's answer and the commands
    /// view close (D155). `false` when a read or a setup is out.
    pub(super) fn again(self) -> bool {
        self.read_again(true)
    }

    /// A new run, the shown setup's answer kept: after a settings switch
    /// that found nothing to switch (review M5).
    pub(super) fn refresh(self) -> bool {
        self.read_again(false)
    }

    fn read_again(self, clear: bool) -> bool {
        if self.checking.get_untracked() || self.busy.get_untracked() {
            return false;
        }
        self.checking.set(true);
        if clear {
            self.run.set(None);
            self.terminal.set(false);
        }
        spawn_local(async move {
            self.take(read(true).await);
            self.checking.try_set(false);
        });
        true
    }

    /// "Set it up" for the shown plan, whose hash is `hash` (§4.3): the
    /// shell checks again, asks in a dialog of its own, then the computer
    /// asks for the password. Nothing while a setup or a read is out.
    pub(super) fn set_up(self, hash: String) {
        if self.busy.get_untracked() || self.checking.get_untracked() {
            return;
        }
        let steps = self.check.with_untracked(|c| {
            c.as_ref()
                .and_then(|c| c.plan.as_ref())
                .map(|p| p.steps.clone())
                .unwrap_or_default()
        });
        self.run.set(Some(Progress::new(&steps)));
        self.terminal.set(false);
        // The last run's callback goes before the next one is made (never
        // from inside its own call).
        if let Some(l) = self.listener.try_update_value(Option::take).flatten() {
            l.stop();
        }
        let (run, listener) = (self.run, self.listener);
        spawn_local(async move {
            let on_event = move |ev| {
                // After the answer an event changes nothing (D154), and
                // nothing that reads the run is told of it (review L4).
                let open = run
                    .try_with_untracked(|r| r.as_ref().is_some_and(Progress::running))
                    .unwrap_or(false);
                if open {
                    run.update(|r| {
                        if let Some(p) = r {
                            p.take(ev);
                        }
                    });
                }
            };
            let keep = move |l: bridge::Listener| {
                // The state went while the call was made: nobody follows.
                if let Some(Some(l)) = listener.try_set_value(Some(l)) {
                    l.stop();
                }
            };
            let answer = bridge::apply(&hash, on_event, keep).await;
            if let Ok(a) = &answer {
                if let Some(c) = &a.check {
                    self.check.try_set(Some(c.clone()));
                    self.error.try_set(None);
                }
                self.terminal
                    .try_set(a.outcome == MachineApplyOutcome::NoAgent);
            }
            run.try_update(|r| {
                if let Some(p) = r {
                    p.ended(answer);
                }
            });
        });
    }
}
