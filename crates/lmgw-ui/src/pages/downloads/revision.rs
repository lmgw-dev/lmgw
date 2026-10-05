//! Which revision a download asked for and which commit its file came from,
//! as the Downloads table shows it.
//!
//! An audio catalog download may take the commit a spec pins instead of
//! `main`, and every download now records the commit the hub resolved it
//! to. A row from before that was recorded says "unknown" — never a guess.

use lmgw_api_types::DownloadRow;

/// The first seven characters of a commit, as git and the hub show it.
fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// A full commit hash, as opposed to `main`.
fn is_commit(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `(the short text beside the file name, the full sentence for its title)`.
/// The short text is empty where there is nothing to say yet (a `main`
/// download still on its way).
pub(super) fn revision_note(d: &DownloadRow) -> (String, String) {
    let asked = d.requested_revision.as_deref();
    let pinned = asked.is_some_and(is_commit);
    let downloaded = matches!(d.status.as_str(), "done" | "update_available");
    match (downloaded, d.resolved_commit.as_deref(), asked) {
        (true, Some(c), _) => (
            match pinned {
                true => format!("@ {} · pinned", short(c)),
                false => format!("@ {}", short(c)),
            },
            format!(
                "downloaded from commit {c}, asked for {}",
                match pinned {
                    true => "the commit the audio.cpp spec pins",
                    false => asked.unwrap_or("main"),
                }
            ),
        ),
        (true, None, None) => (
            "@ unknown commit".to_string(),
            "downloaded before lmgw recorded which commit a file came from".to_string(),
        ),
        (true, None, Some(a)) => (
            "@ unknown commit".to_string(),
            format!("the hub did not name the commit it resolved {a} to"),
        ),
        (false, _, Some(a)) if pinned => (
            format!("at {} · pinned", short(a)),
            format!("asks for commit {a}, the one the audio.cpp spec pins"),
        ),
        (false, _, _) => (String::new(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIN: &str = "607a30d783dfa663caf39e06633721c8d4cfcd7e";

    fn row(status: &str, asked: Option<&str>, commit: Option<&str>) -> DownloadRow {
        DownloadRow {
            status: status.into(),
            requested_revision: asked.map(String::from),
            resolved_commit: commit.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn the_table_says_what_was_asked_and_what_came() {
        assert_eq!(
            revision_note(&row("done", Some(PIN), Some(PIN))).0,
            "@ 607a30d · pinned"
        );
        let (short, long) = revision_note(&row("done", Some("main"), Some(PIN)));
        assert_eq!(short, "@ 607a30d");
        assert!(long.ends_with("asked for main"), "{long}");
        let (short, long) = revision_note(&row("done", None, None));
        assert_eq!(short, "@ unknown commit");
        assert!(long.contains("before lmgw recorded"), "{long}");
        assert_eq!(
            revision_note(&row("queued", Some(PIN), None)).0,
            "at 607a30d · pinned"
        );
        assert_eq!(revision_note(&row("queued", Some("main"), None)).0, "");
    }
}
