//! A thread's two languages (chat-voice design §2.1–§2.2, split
//! 2026-10-05): `voice.language`, the one the user speaks, and
//! `voice.reply_language`, the one replies are in. Both take the same
//! shapes — an ISO 639-1 code, `auto`, or empty to inherit — and differ only
//! in what `auto` means, which their refusals say.

use super::{is_language_code, FOLLOW_USER};

/// What `auto` means on `voice.language`, for its refusal.
pub(super) const SPOKEN_AUTO: &str = "the speech-to-text model detects the language";

/// What `auto` means on `voice.reply_language`, for its refusal.
pub(super) const REPLY_AUTO: &str = "replies follow the language you speak";

/// Fold an already trimmed language `v` to lowercase and refuse anything
/// but an ISO 639-1 code or [`FOLLOW_USER`]; `key` names it, `auto` says
/// what `auto` does there.
pub(super) fn check(v: &mut Option<String>, key: &str, auto: &str) -> Result<(), String> {
    let Some(l) = v.as_mut() else {
        return Ok(());
    };
    *l = l.to_ascii_lowercase();
    if is_language_code(l) || l == FOLLOW_USER {
        return Ok(());
    }
    Err(format!(
        "{key} '{l}' is not an ISO 639-1 code (two letters, such as de or en) or \
         '{FOLLOW_USER}' ({auto}); leave it empty to inherit"
    ))
}

#[cfg(test)]
mod tests {
    use super::super::ThreadVoice;
    use super::*;

    #[test]
    fn the_reply_language_takes_the_spoken_one_s_shapes() {
        let mut v = ThreadVoice {
            language: Some(" DE ".into()),
            reply_language: Some(" En ".into()),
            ..Default::default()
        };
        v.normalise().unwrap();
        assert_eq!(
            (v.language.as_deref(), v.reply_language.as_deref()),
            (Some("de"), Some("en"))
        );
        let mut v = ThreadVoice {
            reply_language: Some(" AUTO ".into()),
            ..Default::default()
        };
        v.normalise().unwrap();
        assert_eq!(v.reply_language.as_deref(), Some(FOLLOW_USER));
        let mut v = ThreadVoice {
            reply_language: Some("  ".into()),
            ..Default::default()
        };
        v.normalise().unwrap();
        assert_eq!(v.reply_language, None, "empty inherits");
        for bad in ["eng", "e", "en-GB", "automatic"] {
            let mut v = ThreadVoice {
                reply_language: Some(bad.into()),
                ..Default::default()
            };
            let e = v.normalise().unwrap_err();
            assert!(e.starts_with("voice.reply_language"), "{e}");
            assert!(e.contains(REPLY_AUTO), "{e}");
        }
    }

    #[test]
    fn a_stored_reply_language_reads_and_overlays() {
        let v = ThreadVoice::from_stored(r#"{"language":"de","reply_language":"en"}"#);
        assert_eq!(v.reply_language.as_deref(), Some("en"));
        assert_eq!(v.to_stored(), r#"{"language":"de","reply_language":"en"}"#);
        let mut t = ThreadVoice {
            reply_language: Some("fr".into()),
            ..Default::default()
        };
        t.overlay(&ThreadVoice {
            reply_language: Some("en".into()),
            ..Default::default()
        });
        assert_eq!(t.reply_language.as_deref(), Some("en"));
        t.overlay(&ThreadVoice::default());
        assert_eq!(t.reply_language.as_deref(), Some("en"), "absent keeps");
    }
}
