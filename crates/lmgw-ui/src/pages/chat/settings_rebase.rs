//! The open thread's settings form, rebased onto a stored row another
//! writer changed while the owner edits the form (review CL-8): every field
//! the owner did not touch (it still shows what the page held) takes the
//! stored value, and an edited one keeps the edit. A Save then writes the
//! owner's edits over the other writer's change, not the whole form back as
//! it was. Field by field for the text and number boxes, sampling's
//! included; as a whole for the MCP servers, the knowledge bases and the
//! voice.

use leptos::prelude::*;

use super::super::chat_sampling::SamplingText;
use super::{same_number, ChatThread, SettingsDraft, SettingsText};

/// One box: the stored value while it still shows the held one (`same`
/// as a save reads them), the owner's edit otherwise.
fn field(form: &str, held: &str, stored: &str, same: fn(&str, &str) -> bool) -> String {
    if same(form, held) {
        stored.to_string()
    } else {
        form.to_string()
    }
}

fn exact(a: &str, b: &str) -> bool {
    a == b
}

fn trimmed(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

/// Stop sequences as the list they make, as `SamplingText::differs` reads
/// them.
fn same_stops(a: &str, b: &str) -> bool {
    let of = |stop: &str| SamplingText {
        stop: stop.to_string(),
        ..Default::default()
    };
    !of(a).differs(&of(b))
}

/// `form` rebased from `held` onto `stored`.
fn rebase_text(form: &SettingsText, held: &SettingsText, stored: &SettingsText) -> SettingsText {
    let (f, h, s) = (&form.sampling, &held.sampling, &stored.sampling);
    SettingsText {
        sys: field(&form.sys, &held.sys, &stored.sys, exact),
        temp: field(&form.temp, &held.temp, &stored.temp, same_number::<f64>),
        max_tok: field(
            &form.max_tok,
            &held.max_tok,
            &stored.max_tok,
            same_number::<i64>,
        ),
        think: field(&form.think, &held.think, &stored.think, trimmed),
        effort: field(&form.effort, &held.effort, &stored.effort, trimmed),
        budget: field(
            &form.budget,
            &held.budget,
            &stored.budget,
            same_number::<i64>,
        ),
        sampling: SamplingText {
            top_p: field(&f.top_p, &h.top_p, &s.top_p, same_number::<f64>),
            top_k: field(&f.top_k, &h.top_k, &s.top_k, same_number::<i64>),
            min_p: field(&f.min_p, &h.min_p, &s.min_p, same_number::<f64>),
            repeat: field(&f.repeat, &h.repeat, &s.repeat, same_number::<f64>),
            presence: field(&f.presence, &h.presence, &s.presence, same_number::<f64>),
            frequency: field(&f.frequency, &h.frequency, &s.frequency, same_number::<f64>),
            seed: field(&f.seed, &h.seed, &s.seed, same_number::<i64>),
            stop: field(&f.stop, &h.stop, &s.stop, same_stops),
        },
    }
}

fn set_if<T: PartialEq + Send + Sync + 'static>(s: RwSignal<T>, v: T) {
    if s.with_untracked(|cur| *cur != v) {
        s.set(v);
    }
}

impl SettingsDraft {
    /// The form rebased from the row the page held (`held`) onto the one
    /// stored now (`stored`), while the owner edits it (module doc).
    pub(in crate::pages) fn rebase(&self, held: &ChatThread, stored: &ChatThread) {
        untrack(|| {
            let next = rebase_text(
                &self.text(),
                &SettingsText::of(held),
                &SettingsText::of(stored),
            );
            set_if(self.sys, next.sys);
            set_if(self.temp, next.temp);
            set_if(self.max_tok, next.max_tok);
            set_if(self.think, next.think);
            set_if(self.effort, next.effort);
            set_if(self.budget, next.budget);
            if self.sampling.text() != next.sampling {
                self.sampling.seed(next.sampling);
            }
            if self.picked.with(|p| *p == held.mcp_tools) {
                set_if(self.picked, stored.mcp_tools.clone());
            }
            if !self.kb.differs_from(held) {
                self.kb.seed(stored);
            }
            if !self.voice.differs_from(&held.voice) {
                self.voice.load(&stored.voice);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(sys: &str, temp: &str, top_p: &str, stop: &str) -> SettingsText {
        SettingsText {
            sys: sys.into(),
            temp: temp.into(),
            sampling: SamplingText {
                top_p: top_p.into(),
                stop: stop.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The review's case: the owner edits the prompt while another writer
    /// changes the temperature. The prompt stays the owner's, the
    /// temperature follows, so a Save does not write the old one back.
    #[test]
    fn untouched_fields_follow_the_stored_row_and_edits_stay() {
        let held = text("be brief", "0.7", "0.9", "END");
        let form = text("be very brief", "0.70", "0.9", "END");
        let stored = text("be brief", "0.2", "0.5", "STOP");
        let next = rebase_text(&form, &held, &stored);
        assert_eq!(next.sys, "be very brief", "the owner's edit stays");
        assert_eq!(next.temp, "0.2", "0.70 is the 0.7 held: untouched");
        assert_eq!(next.sampling.top_p, "0.5");
        assert_eq!(next.sampling.stop, "STOP");
    }

    #[test]
    fn an_edit_to_a_field_the_other_writer_changed_too_is_the_owner_s() {
        let held = text("a", "0.7", "", "");
        let form = text("a", "1.0", "0.8", "X");
        let stored = text("b", "0.2", "0.5", "Y");
        let next = rebase_text(&form, &held, &stored);
        assert_eq!(next.sys, "b");
        assert_eq!(next.temp, "1.0");
        assert_eq!(next.sampling.top_p, "0.8");
        assert_eq!(next.sampling.stop, "X");
        // Nothing edited: the stored row whole.
        assert_eq!(rebase_text(&held, &held, &stored), stored);
    }
}
