//! The realtime panel's keys (chat-voice §9.2), as pure rules tested
//! natively: Space (push-to-talk held; stop talking in automatic mode), M
//! (mute) and Esc (leave).
//!
//! They act only when the focus is on the panel or the page itself
//! ([`Place`]), never in a text field — anywhere else on the page keeps its
//! own keys. A modified key (Ctrl, Alt, Meta) is a shortcut, never one of
//! these, and an IME composition keeps its keys. Esc leaves only with no
//! dialog or popover open: theirs is the Esc.

/// Where the focus is when a key comes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// The page itself: nothing focused (`body`).
    Page,
    /// The panel or a control in it.
    Panel,
    /// A text field anywhere (an input, a textarea, contenteditable).
    Text,
    /// Some other control of the page (the sidebar, a message's actions).
    Elsewhere,
}

/// What a key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Act {
    /// Not ours: its default goes ahead.
    Pass,
    /// Ours, and it does nothing more (Space's auto-repeat): its default is
    /// stopped, so a focused button is not pressed.
    Swallow,
    /// Space went down.
    TalkDown,
    Mute,
    Leave,
}

/// One key going down.
pub(crate) struct Down<'a> {
    pub key: &'a str,
    pub code: &'a str,
    pub repeat: bool,
    pub composing: bool,
    /// Ctrl, Alt or Meta held.
    pub modified: bool,
    pub place: Place,
    /// A `<dialog>` or a popover of the page is open.
    pub dialog_open: bool,
}

pub(crate) fn is_space(key: &str, code: &str) -> bool {
    code == "Space" || key == " "
}

pub(crate) fn key_down(d: &Down) -> Act {
    if d.composing || d.modified || !matches!(d.place, Place::Page | Place::Panel) {
        return Act::Pass;
    }
    if is_space(d.key, d.code) {
        return if d.repeat {
            Act::Swallow
        } else {
            Act::TalkDown
        };
    }
    if d.repeat {
        return Act::Pass;
    }
    if d.key.eq_ignore_ascii_case("m") {
        return Act::Mute;
    }
    if d.key == "Escape" && !d.dialog_open {
        return Act::Leave;
    }
    Act::Pass
}

/// Does this key-up end a held push-to-talk? Space, wherever the focus went
/// meanwhile (its key-down was ours).
pub(crate) fn key_up_ends_talk(key: &str, code: &str, held: bool) -> bool {
    held && is_space(key, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(key: &str, place: Place) -> Down<'_> {
        Down {
            key,
            code: if key == " " { "Space" } else { "" },
            repeat: false,
            composing: false,
            modified: false,
            place,
            dialog_open: false,
        }
    }

    #[test]
    fn space_m_and_esc_act_on_the_page_and_the_panel() {
        for place in [Place::Page, Place::Panel] {
            assert_eq!(key_down(&down(" ", place)), Act::TalkDown);
            assert_eq!(key_down(&down("m", place)), Act::Mute);
            assert_eq!(key_down(&down("M", place)), Act::Mute);
            assert_eq!(key_down(&down("Escape", place)), Act::Leave);
            assert_eq!(key_down(&down("x", place)), Act::Pass);
        }
    }

    #[test]
    fn never_in_a_text_field_nor_on_another_control() {
        for place in [Place::Text, Place::Elsewhere] {
            for k in [" ", "m", "Escape"] {
                assert_eq!(key_down(&down(k, place)), Act::Pass, "{k} at {place:?}");
            }
        }
    }

    #[test]
    fn shortcuts_compositions_and_repeats() {
        let mut d = down(" ", Place::Page);
        d.repeat = true;
        assert_eq!(key_down(&d), Act::Swallow, "a held Space presses nothing");
        let mut d = down("m", Place::Page);
        d.repeat = true;
        assert_eq!(key_down(&d), Act::Pass);
        let mut d = down("m", Place::Page);
        d.modified = true;
        assert_eq!(key_down(&d), Act::Pass, "Ctrl+M is a shortcut");
        let mut d = down(" ", Place::Panel);
        d.composing = true;
        assert_eq!(key_down(&d), Act::Pass);
        // Space by its position: a layout that sends another key value.
        let d = Down {
            key: "Spacebar",
            code: "Space",
            ..down(" ", Place::Page)
        };
        assert_eq!(key_down(&d), Act::TalkDown);
    }

    #[test]
    fn esc_is_a_dialogs_while_one_is_open() {
        let mut d = down("Escape", Place::Page);
        d.dialog_open = true;
        assert_eq!(key_down(&d), Act::Pass);
    }

    #[test]
    fn space_up_ends_a_held_talk() {
        assert!(key_up_ends_talk(" ", "Space", true));
        assert!(!key_up_ends_talk(" ", "Space", false));
        assert!(!key_up_ends_talk("m", "KeyM", true));
    }
}
