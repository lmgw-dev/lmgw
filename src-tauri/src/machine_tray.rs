//! The tray's word on the graphics card (machine-check design §2.4, WP9).
//!
//! The tray learns the machine summary the way it learns everything else, from
//! the in-process state on its 5 s tick (`state.machine.summary()`), so there is
//! no HTTP and no second reader. [`Watch`] is the decision alone, with no
//! Tauri in it: it is told what the summary says now and answers what the
//! menu has to do about it.
//!
//! The rule is the doc's: the item appears when the verdict goes from `ready`
//! to anything else, and goes when it is `ready` again. A computer that has
//! not been `ready` since lmgw started shows nothing here, because the window
//! that opens at every launch already shows a newcomer the check (§2.1, D105).

use lmgw_api_types::machine::{MachineSummary, MachineVerdict};

/// The menu item's label.
pub(crate) const ITEM_LABEL: &str = "Graphics card needs attention…";

/// Where the item opens the window (the section is the Settings card, WP8a).
pub(crate) const ITEM_ROUTE: &str = "/settings/gpu#machine";

/// What the menu has to do after a tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    /// Nothing differs from the last tick.
    None,
    /// The item is not there yet: add it, and say this sentence.
    Show(String),
    /// The item is there and the sentence changed.
    Reword(String),
    /// The verdict is `ready` again: take the item out.
    Hide,
}

/// What the tray has seen of the machine so far.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    /// The verdict has been `ready` at some point since lmgw started.
    was_ready: bool,
    /// The sentence the item currently shows; `None` while it is not there.
    shown: Option<String>,
}

impl Watch {
    /// The sentence the status line and tooltip name, while the item is there.
    pub(crate) fn sentence(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    /// Takes the summary as it is now (`None` before the boot check finished)
    /// and says what changes.
    pub(crate) fn observe(&mut self, summary: Option<&MachineSummary>) -> Change {
        let want = match summary {
            // Nothing is known yet, so nothing is shown.
            None => None,
            Some(s) if s.verdict == MachineVerdict::Ready => {
                self.was_ready = true;
                None
            }
            Some(s) => self.was_ready.then(|| s.headline.clone()),
        };
        match (self.shown.take(), want) {
            (None, None) => Change::None,
            (None, Some(now)) => {
                self.shown = Some(now.clone());
                Change::Show(now)
            }
            (Some(_), None) => Change::Hide,
            (Some(before), Some(now)) if before == now => {
                self.shown = Some(before);
                Change::None
            }
            (Some(_), Some(now)) => {
                self.shown = Some(now.clone());
                Change::Reword(now)
            }
        }
    }
}

/// The status line with the sentence after it, when there is one.
pub(crate) fn status_line(base: &str, sentence: Option<&str>) -> String {
    match sentence {
        Some(s) => format!("{base} · {s}"),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(verdict: MachineVerdict, headline: &str) -> MachineSummary {
        MachineSummary {
            verdict,
            headline: headline.into(),
            checked_at: "2026-11-03 08:00:00".into(),
            failed_start: None,
        }
    }
    const RESTART: &str = "Restart the computer to finish the graphics driver update.";

    #[test]
    fn nothing_is_shown_before_the_boot_check_finished() {
        let mut w = Watch::default();
        assert_eq!(w.observe(None), Change::None);
        assert_eq!(w.sentence(), None);
    }

    #[test]
    fn ready_shows_nothing_and_arms_the_item() {
        let mut w = Watch::default();
        assert_eq!(w.observe(None), Change::None);
        assert_eq!(
            w.observe(Some(&s(MachineVerdict::Ready, "ready"))),
            Change::None
        );
        assert_eq!(
            w.observe(Some(&s(MachineVerdict::NeedsRestart, RESTART))),
            Change::Show(RESTART.into())
        );
        assert_eq!(w.sentence(), Some(RESTART));
    }

    #[test]
    fn a_verdict_that_stays_changes_nothing() {
        let mut w = Watch::default();
        w.observe(Some(&s(MachineVerdict::Ready, "ready")));
        w.observe(Some(&s(MachineVerdict::NeedsRestart, RESTART)));
        assert_eq!(
            w.observe(Some(&s(MachineVerdict::NeedsRestart, RESTART))),
            Change::None
        );
    }

    #[test]
    fn ready_again_takes_the_item_out() {
        let mut w = Watch::default();
        w.observe(Some(&s(MachineVerdict::Ready, "ready")));
        w.observe(Some(&s(MachineVerdict::NeedsFix, "one thing")));
        assert_eq!(
            w.observe(Some(&s(MachineVerdict::Ready, "ready"))),
            Change::Hide
        );
        assert_eq!(w.sentence(), None);
        assert_eq!(
            w.observe(Some(&s(MachineVerdict::Ready, "ready"))),
            Change::None
        );
        // And it comes back when the card stops working again.
        assert_eq!(
            w.observe(Some(&s(MachineVerdict::NeedsDriver, "driver"))),
            Change::Show("driver".into())
        );
    }

    #[test]
    fn a_machine_never_ready_since_start_shows_nothing() {
        let mut w = Watch::default();
        for v in [
            MachineVerdict::NeedsFix,
            MachineVerdict::NeedsSteps,
            MachineVerdict::NeedsRestart,
            MachineVerdict::NeedsDriver,
            MachineVerdict::CloudOnly,
        ] {
            assert_eq!(w.observe(Some(&s(v, "x"))), Change::None);
        }
        assert_eq!(w.sentence(), None);
        // The first `ready` arms it: what follows is "from ready".
        w.observe(Some(&s(MachineVerdict::Ready, "ready")));
        assert_eq!(
            w.observe(Some(&s(MachineVerdict::CloudOnly, "none"))),
            Change::Show("none".into())
        );
    }

    #[test]
    fn a_new_sentence_while_not_ready_rewords_the_item() {
        let mut w = Watch::default();
        w.observe(Some(&s(MachineVerdict::Ready, "ready")));
        w.observe(Some(&s(MachineVerdict::NeedsFix, "one thing")));
        assert_eq!(
            w.observe(Some(&s(MachineVerdict::NeedsRestart, RESTART))),
            Change::Reword(RESTART.into())
        );
        assert_eq!(w.sentence(), Some(RESTART));
    }

    #[test]
    fn the_status_line_names_the_sentence_only_while_there_is_one() {
        assert_eq!(status_line("lmgw · x", None), "lmgw · x");
        assert_eq!(
            status_line("lmgw · x", Some("Restart.")),
            "lmgw · x · Restart."
        );
    }
}
