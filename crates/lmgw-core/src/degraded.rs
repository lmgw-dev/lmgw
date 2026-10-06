//! What a request's content lost on its way to a model that lacks a
//! capability (the owner's requirement of 2026-10-06): one marker on its
//! request row (`request_logs.degraded`), shown on the Traffic page and in
//! the row's JSON.
//!
//! A configured fallback is always used, a candidate alias's that lacks a
//! facet the alias enables too, and what the model that answers cannot take
//! goes to it in a form it can: images as placeholders
//! (`gate::fallback_images`), a Chat PDF's pages as its text, a voice turn
//! as its transcript, and the Chat's own attachments as notes or
//! transcripts for a thread model that cannot see or hear them. Each of
//! those says what it did here, in one shape — "fallback 'x' lacks vision: 3
//! images sent as placeholders", "'m' lacks audio: transcript sent" — and a
//! send that did more than one joins them ([`join`]). The marker says what
//! the request lost, not why the route was taken: the row's
//! `fallback_reason` says that.

/// One degradation: `who` (a model's name), whether it answers as a
/// fallback, the `capability` it lacks, and what went to it instead.
pub fn lacks(who: &str, fallback: bool, capability: &str, sent: &str) -> String {
    let who = if fallback {
        format!("fallback '{who}'")
    } else {
        format!("'{who}'")
    };
    format!("{who} lacks {capability}: {sent}")
}

/// `n` images, with `as_what` after them in the matching number ("1 image
/// sent as a placeholder", "3 images sent as placeholders").
pub fn images(n: usize, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 image sent as {one}"),
        n => format!("{n} images sent as {many}"),
    }
}

/// The markers of one request, joined, each once and in order: `None` when
/// there is none.
pub fn join<I>(parts: I) -> Option<String>
where
    I: IntoIterator<Item = Option<String>>,
{
    let mut out: Vec<String> = Vec::new();
    for part in parts.into_iter().flatten() {
        for one in part.split("; ") {
            if !one.is_empty() && !out.iter().any(|o| o == one) {
                out.push(one.to_string());
            }
        }
    }
    (!out.is_empty()).then(|| out.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_says_who_lacks_what_and_what_went_instead() {
        assert_eq!(
            lacks(
                "cloud",
                true,
                "vision",
                &images(3, "a placeholder", "placeholders")
            ),
            "fallback 'cloud' lacks vision: 3 images sent as placeholders"
        );
        assert_eq!(
            lacks("m", false, "audio", "transcript sent"),
            "'m' lacks audio: transcript sent"
        );
        assert_eq!(images(1, "a note", "notes"), "1 image sent as a note");
    }

    #[test]
    fn markers_join_each_once_in_order() {
        assert_eq!(join([None, None]), None);
        assert_eq!(
            join([
                Some("a; b".to_string()),
                None,
                Some("b".to_string()),
                Some("c".to_string())
            ])
            .as_deref(),
            Some("a; b; c")
        );
    }
}
