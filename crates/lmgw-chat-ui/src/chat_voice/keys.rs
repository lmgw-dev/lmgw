//! Hold-to-dictate on the keyboard (chat-voice design §5): **Right Ctrl**,
//! anywhere on the Chat page. Pure, so it is tested natively; the listeners
//! are the page's (`page.rs`).
//!
//! - The key is matched by its position (`code == "ControlRight"`) *and* its
//!   meaning (`key == "Control"`): a Right Ctrl remapped by the layout — the
//!   Compose key, a layout switch, a third-level shift (AltGr) — reports
//!   another `key` and is not dictation (WP7 review M1). Nor is a press
//!   during an IME composition.
//! - Key repeats are ignored.
//! - Another key pressed meanwhile cancels the recording, so Ctrl+C stays
//!   Ctrl+C (and Ctrl+Enter, Ctrl+± zoom in the app): the other key keeps
//!   its own effect, nothing is sent. Within the key's arm time nothing was
//!   opened yet, so a shortcut cancels without a word (`dictation.rs`).
//! - Esc while recording or transcribing discards the audio, whatever
//!   started it. Esc while the key still arms is a combination like any
//!   other: nothing was opened, so it cancels without a word and keeps its
//!   own effect (an open popover closes, WP11 UI review NIT 10).
//!
//! No other shortcut of the dashboard uses Right Ctrl alone: the app's zoom
//! is Ctrl with `+`/`-`/`0` or the wheel, the message editor saves on
//! Ctrl+Enter, the lists' filters take `/` — each a combination, which this
//! cancels and lets through. The key is one constant, so it is easy to
//! change.

/// `KeyboardEvent.code` of the hold-to-dictate key: its position.
pub(crate) const DICTATE_KEY: &str = "ControlRight";
/// `KeyboardEvent.key` it must report: Right Ctrl still meaning Ctrl.
pub(crate) const DICTATE_KEY_NAME: &str = "Control";
/// Its name in tooltips.
pub(crate) const DICTATE_KEY_LABEL: &str = "Right Ctrl";

/// Where dictation is, as the keys see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    /// The key is down, its arm time not over: nothing is open yet.
    Arming,
    /// Opening or recording, started by holding the key.
    HeldByKey,
    /// Recording started by the button, or transcribing.
    Busy,
}

impl Phase {
    /// The key holds it: its release ends it, and another key, a pointer
    /// press or the wheel is a combination.
    pub(crate) fn held(self) -> bool {
        matches!(self, Phase::Arming | Phase::HeldByKey)
    }
}

/// What a key press does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyDown {
    Nothing,
    /// Start recording, held until the key comes up.
    Start,
    /// Discard the audio: Esc.
    Discard,
    /// Another key while the dictation key is held: cancel, and let that
    /// key do its own thing.
    Cancel,
}

/// `composing`: `KeyboardEvent.isComposing` (an IME composition is open).
pub(crate) fn key_down(
    code: &str,
    key: &str,
    repeat: bool,
    composing: bool,
    phase: Phase,
) -> KeyDown {
    if code == DICTATE_KEY && key == DICTATE_KEY_NAME {
        return match (repeat || composing, phase) {
            (false, Phase::Idle) => KeyDown::Start,
            _ => KeyDown::Nothing,
        };
    }
    match phase {
        Phase::Idle => KeyDown::Nothing,
        // Nothing was opened: a combination, silent, Esc's own effect kept.
        Phase::Arming => KeyDown::Cancel,
        _ if key == "Escape" => KeyDown::Discard,
        Phase::HeldByKey => KeyDown::Cancel,
        Phase::Busy => KeyDown::Nothing,
    }
}

/// Whether a key coming up ends a recording the key started.
pub(crate) fn key_up_releases(code: &str, phase: Phase) -> bool {
    code == DICTATE_KEY && phase.held()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_ctrl_starts_once_and_repeats_are_ignored() {
        assert_eq!(
            key_down("ControlRight", "Control", false, false, Phase::Idle),
            KeyDown::Start
        );
        assert_eq!(
            key_down("ControlRight", "Control", true, false, Phase::Idle),
            KeyDown::Nothing,
            "a repeat never starts one"
        );
        assert_eq!(
            key_down("ControlRight", "Control", true, false, Phase::HeldByKey),
            KeyDown::Nothing
        );
        assert_eq!(
            key_down("ControlLeft", "Control", false, false, Phase::Idle),
            KeyDown::Nothing,
            "left Ctrl is not the key"
        );
        assert!(key_up_releases("ControlRight", Phase::HeldByKey));
        assert!(key_up_releases("ControlRight", Phase::Arming));
        assert!(
            !key_up_releases("ControlRight", Phase::Busy),
            "the button's recording goes on"
        );
        assert!(!key_up_releases("ControlLeft", Phase::HeldByKey));
    }

    #[test]
    fn a_remapped_right_ctrl_is_not_the_key() {
        // KDE's "Compose key: Right Ctrl", a layout switch on Right Ctrl, and
        // Right Ctrl as the third-level shift report another `key`.
        for key in ["Compose", "GroupNext", "AltGraph", "Multi"] {
            assert_eq!(
                key_down("ControlRight", key, false, false, Phase::Idle),
                KeyDown::Nothing,
                "{key}"
            );
            // While Right Ctrl (as Ctrl) is held it is another key.
            assert_eq!(
                key_down("ControlRight", key, false, false, Phase::HeldByKey),
                KeyDown::Cancel,
                "{key}"
            );
        }
        // AltGr itself (EurKEY, German layouts) is its own key.
        assert_eq!(
            key_down("AltRight", "AltGraph", false, false, Phase::Idle),
            KeyDown::Nothing
        );
        // An IME composition keeps its keys.
        assert_eq!(
            key_down("ControlRight", "Control", false, true, Phase::Idle),
            KeyDown::Nothing
        );
    }

    #[test]
    fn another_key_cancels_so_ctrl_c_stays_ctrl_c() {
        assert_eq!(
            key_down("KeyC", "c", false, false, Phase::HeldByKey),
            KeyDown::Cancel
        );
        assert_eq!(
            key_down("Enter", "Enter", false, false, Phase::HeldByKey),
            KeyDown::Cancel
        );
        assert_eq!(
            key_down("KeyC", "c", false, false, Phase::Busy),
            KeyDown::Nothing
        );
        assert_eq!(
            key_down("KeyC", "c", false, false, Phase::Idle),
            KeyDown::Nothing
        );
    }

    #[test]
    fn a_key_while_right_ctrl_arms_is_a_silent_combination() {
        for (code, key) in [("KeyC", "c"), ("Escape", "Escape"), ("ShiftLeft", "Shift")] {
            assert_eq!(
                key_down(code, key, false, false, Phase::Arming),
                KeyDown::Cancel,
                "{key}"
            );
        }
        assert!(Phase::Arming.held() && Phase::HeldByKey.held());
        assert!(!Phase::Busy.held() && !Phase::Idle.held());
    }

    #[test]
    fn esc_discards_whatever_started_it() {
        assert_eq!(
            key_down("Escape", "Escape", false, false, Phase::HeldByKey),
            KeyDown::Discard
        );
        assert_eq!(
            key_down("Escape", "Escape", false, false, Phase::Busy),
            KeyDown::Discard
        );
        assert_eq!(
            key_down("Escape", "Escape", false, false, Phase::Idle),
            KeyDown::Nothing
        );
    }
}
