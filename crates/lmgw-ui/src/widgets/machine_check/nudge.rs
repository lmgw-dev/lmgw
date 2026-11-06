//! When the check reaches out beyond Settings (machine-check design §2.1,
//! §2.2, D170 to D173): the Overview's card, the Chat's empty-state block
//! and its banner show while the verdict wants something done (a `needs_*`)
//! and local models are involved, unless the person said "Not now" to the
//! check as it is.
//!
//! - **Involved** is the dashboard's to say, from the admin plane's rows
//!   ([`super::involved`], D184): a local model that runs on the graphics
//!   card is configured, or nothing is configured at all yet (a first
//!   launch, where the person may still pick a local one). A setup of cloud
//!   models only is not nagged (§2.2).
//! - **"Not now"** is one answer for the three surfaces, stored in the
//!   browser (`lmgw.ui.machine.not_now`, the app window's own storage in the
//!   app), as the check's state it was said to ([`state_key`]) and the
//!   failed start it was said beside ([`failed_key`]). It holds until the
//!   check reads otherwise (another verdict, another problem, a problem
//!   gone; the check's time, its wording, its details and its plan's hash do
//!   not count) or carries another failed start: a local model's start on
//!   the graphics card that failed because of this computer, whoever asked
//!   for it (a client, the Chat, a lab, the model's Test), which core stamps
//!   on the check (D182). A state that changes and comes back is asked about
//!   again: the stored answer goes the moment the shown check reads
//!   otherwise ([`Nudge::settle`]).

use leptos::prelude::*;
use lmgw_api_types::machine::{ItemState, MachineCheck, MachineVerdict};

use super::MachineState;

/// Where "Not now" is kept, under the kit's `lmgw.ui.` prefix.
const NOT_NOW_KEY: &str = "machine.not_now";

/// Between the state and the failed start in the stored answer; neither
/// ever has one.
const SEP: char = '@';

/// Does the verdict want something done? `ready` and `cloud_only` do not.
pub(crate) fn needs(v: MachineVerdict) -> bool {
    !matches!(v, MachineVerdict::Ready | MachineVerdict::CloudOnly)
}

/// The check's state as "Not now" is said to it: the verdict, then each
/// problem item's id with its cause's kind, in the check's order. Never the
/// time it ran, its sentences, its detail or its plan.
pub(crate) fn state_key(c: &MachineCheck) -> String {
    let mut key = word(&c.verdict);
    for it in c.items.iter().filter(|i| i.state == ItemState::Problem) {
        key.push('|');
        key.push_str(&word(&it.id));
        if let Some(cause) = &it.cause {
            key.push(':');
            key.push_str(&cause_kind(cause));
        }
    }
    key
}

/// The failed start the check carries, as "Not now" remembers it: when it
/// failed and how many had failed by then, on lmgw's clock, never the
/// browser's (which may be another computer's). Empty without one.
pub(crate) fn failed_key(c: &MachineCheck) -> String {
    c.failed_start
        .as_ref()
        .map(|f| format!("{} #{}", f.at, f.count))
        .unwrap_or_default()
}

/// The stored answer for the check as it reads: its state and its failed
/// start.
pub(crate) fn said_to(c: &MachineCheck) -> String {
    format!("{}{SEP}{}", state_key(c), failed_key(c))
}

/// A cause's `kind` tag, as the wire names it.
fn cause_kind(c: &lmgw_api_types::machine::MachineCause) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// The snake_case word a unit enum serializes to.
fn word<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Does a stored "Not now" still hold beside the check shown? Nothing said
/// always does, and so does no check known yet; a check that reads
/// otherwise ends it (D171), and so does one that carries a failed start
/// other than the one it was said beside (D182): core only ever stamps a
/// newer one, and a restart of lmgw forgets it, which ends nothing.
pub(crate) fn holds(c: Option<&MachineCheck>, not_now: &str) -> bool {
    let Some(c) = c.filter(|_| !not_now.is_empty()) else {
        return true;
    };
    let (state, failed) = not_now.split_once(SEP).unwrap_or((not_now, ""));
    if state != state_key(c) {
        return false;
    }
    let now = failed_key(c);
    now.is_empty() || now == failed
}

/// Does the check reach out (D170)? A check is known, its verdict wants
/// something done, local models are involved, and "Not now" was not said
/// to the check as it reads (`not_now`: the stored answer, empty when
/// none).
pub(crate) fn reaches(c: Option<&MachineCheck>, involved: bool, not_now: &str) -> bool {
    let Some(c) = c else { return false };
    needs(c.verdict) && involved && (not_now.is_empty() || !holds(Some(c), not_now))
}

/// The surfaces' one "Not now", for the app (provided with the
/// [`MachineState`]).
#[derive(Clone, Copy)]
pub struct Nudge {
    /// The stored answer ([`said_to`]); empty: none.
    not_now: RwSignal<String>,
    /// The check shown, the app's ([`MachineState::check`]).
    check: RwSignal<Option<MachineCheck>>,
}

impl Nudge {
    /// The app's: "Not now" kept in the browser, and forgotten as soon as
    /// the shown check reads otherwise.
    pub(super) fn new(st: MachineState) -> Self {
        let n = Self::of(st.check, crate::prefs::persisted_string(NOT_NOW_KEY, ""));
        Effect::new(move |_| n.settle());
        n
    }

    /// Over these two signals, without storage: a test's.
    fn of(check: RwSignal<Option<MachineCheck>>, not_now: RwSignal<String>) -> Self {
        Self { not_now, check }
    }

    /// The stored answer no longer holds for the check shown (it reads
    /// otherwise, or carries another failed start): it goes, so a state
    /// that comes back is asked about again. Tracks the check; the effect's
    /// body.
    fn settle(self) {
        let stale = self
            .check
            .with(|c| !self.not_now.with_untracked(|n| holds(c.as_ref(), n)));
        if stale {
            self.not_now.set(String::new());
        }
    }

    /// Does the check reach out on a surface where local models are
    /// `involved` (tracked)?
    pub(crate) fn reaches(self, involved: bool) -> bool {
        let n = self.not_now.get();
        self.check.with(|c| reaches(c.as_ref(), involved, &n))
    }

    /// "Not now", said to the check as it is shown.
    pub(crate) fn not_now(self) {
        if let Some(k) = self.check.with_untracked(|c| c.as_ref().map(said_to)) {
            self.not_now.set(k);
        }
    }
}

/// The app's [`Nudge`], or one of this surface's own where none is
/// provided (a test).
pub(crate) fn use_nudge() -> Nudge {
    use_context::<Nudge>().unwrap_or_else(|| Nudge::new(super::use_machine_state()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::machine::{MachineCause, MachineFailedStart, MachineItem, MachineItemId};

    fn item(id: MachineItemId, state: ItemState, cause: Option<MachineCause>) -> MachineItem {
        MachineItem {
            id,
            state,
            sentence: "a sentence".into(),
            detail: vec!["a detail".into()],
            cause,
            action: None,
        }
    }

    fn check(verdict: MachineVerdict, items: Vec<MachineItem>) -> MachineCheck {
        MachineCheck {
            checked_at: "2026-11-03 13:22:07".into(),
            verdict,
            headline: "a headline".into(),
            items,
            plan: None,
            failed_start: None,
        }
    }

    fn toolkit_missing() -> MachineCheck {
        check(
            MachineVerdict::NeedsFix,
            vec![
                item(MachineItemId::GraphicsCard, ItemState::Ok, None),
                item(
                    MachineItemId::CardForModels,
                    ItemState::Problem,
                    Some(MachineCause::ToolkitMissing),
                ),
                item(MachineItemId::Room, ItemState::Info, None),
            ],
        )
    }

    fn restart() -> MachineCheck {
        check(
            MachineVerdict::NeedsRestart,
            vec![
                item(
                    MachineItemId::GraphicsCard,
                    ItemState::Problem,
                    Some(MachineCause::DriverMismatch {
                        kernel: "600.10.01".into(),
                        userspace: "600.20.03".into(),
                    }),
                ),
                item(MachineItemId::CardForModels, ItemState::NotChecked, None),
            ],
        )
    }

    /// `c` after the `count`th start that failed because of the computer,
    /// at `at`: Kai's, an app's, the Test card's, the Chat's alike.
    fn failed(mut c: MachineCheck, at: &str, count: u64) -> MachineCheck {
        c.failed_start = Some(MachineFailedStart {
            model: "qwen-demo-8b".into(),
            class: "chat".into(),
            at: at.into(),
            count,
        });
        c
    }

    #[test]
    fn the_state_is_the_verdict_and_its_problems_with_their_causes() {
        assert_eq!(
            state_key(&toolkit_missing()),
            "needs_fix|card_for_models:toolkit_missing"
        );
        assert_eq!(
            state_key(&restart()),
            "needs_restart|graphics_card:driver_mismatch"
        );
        assert_eq!(state_key(&check(MachineVerdict::Ready, vec![])), "ready");
        assert_eq!(
            said_to(&toolkit_missing()),
            "needs_fix|card_for_models:toolkit_missing@"
        );
        assert_eq!(
            said_to(&failed(restart(), "2026-11-03 14:00:00", 2)),
            "needs_restart|graphics_card:driver_mismatch@2026-11-03 14:00:00 #2"
        );
    }

    #[test]
    fn a_new_run_that_reads_the_same_is_the_same_state() {
        let a = toolkit_missing();
        let mut b = toolkit_missing();
        b.checked_at = "2026-11-04 08:00:00".into();
        b.headline = "the words changed".into();
        b.items[1].detail = vec!["another detail".into()];
        assert_eq!(said_to(&a), said_to(&b));
        assert!(holds(Some(&b), &said_to(&a)));
    }

    #[test]
    fn it_reaches_out_for_a_needs_verdict_where_local_models_are_involved() {
        let c = toolkit_missing();
        assert!(reaches(Some(&c), true, ""));
        assert!(!reaches(None, true, ""), "nothing before a check is known");
        assert!(
            !reaches(Some(&c), false, ""),
            "a cloud-only setup is not nagged (§2.2)"
        );
        for v in [MachineVerdict::Ready, MachineVerdict::CloudOnly] {
            assert!(!reaches(Some(&check(v, vec![])), true, ""), "{v:?}");
        }
        for v in [
            MachineVerdict::NeedsFix,
            MachineVerdict::NeedsSteps,
            MachineVerdict::NeedsRestart,
            MachineVerdict::NeedsDriver,
        ] {
            assert!(reaches(Some(&check(v, vec![])), true, ""), "{v:?}");
        }
    }

    #[test]
    fn not_now_holds_for_the_state_it_was_said_to() {
        let c = toolkit_missing();
        let said = said_to(&c);
        assert!(!reaches(Some(&c), true, &said));
        assert!(holds(Some(&c), &said));
        // The check reads otherwise: the answer is not this state's.
        let r = restart();
        assert!(!holds(Some(&r), &said));
        assert!(reaches(Some(&r), true, &said));
        // Nothing known yet keeps it; nothing said always holds.
        assert!(holds(None, &said));
        assert!(holds(Some(&r), ""));
        // An answer stored before failed starts were remembered is the
        // state alone: it holds for that state.
        assert!(holds(Some(&c), &state_key(&c)));
    }

    /// H1: a start that fails because of the computer after "Not now" ends
    /// it, whoever asked for the start; the one it was said beside does not,
    /// and neither does lmgw forgetting it at a restart.
    #[test]
    fn a_failed_start_after_not_now_ends_it_whoever_made_the_request() {
        let before = failed(toolkit_missing(), "2026-11-03 13:00:00", 1);
        let said = said_to(&before);
        assert!(
            holds(Some(&before), &said),
            "the failure it was said beside"
        );
        assert!(!reaches(Some(&before), true, &said));
        // Kai's request fails afterwards: the check carries that start now.
        let kai = failed(toolkit_missing(), "2026-11-03 15:30:00", 2);
        assert!(!holds(Some(&kai), &said));
        assert!(reaches(Some(&kai), true, &said));
        // Two in the same second are still two.
        let same_second = failed(toolkit_missing(), "2026-11-03 13:00:00", 2);
        assert!(!holds(Some(&same_second), &said));
        // lmgw restarted: no failed start known, nothing ends.
        assert!(holds(Some(&toolkit_missing()), &said));
        // Said with none known, any failed start ends it.
        let said = said_to(&toolkit_missing());
        assert!(!holds(Some(&kai), &said));
    }

    /// The effect's body over the app's two signals: the stored answer goes
    /// the moment the shown check reads otherwise or carries another failed
    /// start, so a state that changes and comes back is asked about again.
    #[test]
    fn the_stored_answer_goes_once_the_check_reads_otherwise() {
        let owner = Owner::new();
        owner.with(|| {
            let shown = RwSignal::new(Some(toolkit_missing()));
            let stored = RwSignal::new(String::new());
            let n = Nudge::of(shown, stored);
            assert!(n.reaches(true));
            n.not_now();
            assert_eq!(
                stored.get_untracked(),
                "needs_fix|card_for_models:toolkit_missing@"
            );
            assert!(!n.reaches(true));
            // A re-run that reads the same keeps it.
            let mut again = toolkit_missing();
            again.checked_at = "2026-11-03 14:00:00".into();
            shown.set(Some(again));
            n.settle();
            assert!(!stored.get_untracked().is_empty());
            assert!(!n.reaches(true));
            // A restart is due now: the answer goes.
            shown.set(Some(restart()));
            n.settle();
            assert_eq!(stored.get_untracked(), "");
            assert!(n.reaches(true));
            // Back to the state it was said to: asked about again.
            shown.set(Some(toolkit_missing()));
            n.settle();
            assert!(n.reaches(true));
            // Said again, then a start fails because of the computer.
            n.not_now();
            assert!(!n.reaches(true));
            shown.set(Some(failed(toolkit_missing(), "2026-11-03 15:30:00", 1)));
            n.settle();
            assert_eq!(stored.get_untracked(), "");
            assert!(n.reaches(true));
            // Nothing configured that the card concerns: never.
            assert!(!n.reaches(false));
        });
    }
}
