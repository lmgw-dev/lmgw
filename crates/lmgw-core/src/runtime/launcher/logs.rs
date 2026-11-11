//! Reading a failed start's log tail — shared by every launcher, and by the
//! benchmark's own container.

/// How many log lines a failed start reads back. Same shape (and same
/// reason) as [`crate::modelinfo`]'s post-mortem tail: with one process per
/// model this excerpt is often the only place the actual failure is visible.
pub const LOG_TAIL: usize = 60;

/// How many of [`LOG_TAIL`]'s lines a failed start's error carries.
pub const LOG_EXCERPT_LINES: usize = 12;

/// A log tail read for a failed start: the last [`LOG_EXCERPT_LINES`] lines
/// that look like a diagnosis, else the last lines as they are — an empty
/// excerpt would leave the error saying nothing at all.
pub fn log_excerpt_of(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = |it: Vec<&str>| -> Vec<String> {
        it.into_iter()
            .rev()
            .take(LOG_EXCERPT_LINES)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    let interesting: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| {
            let low = l.to_ascii_lowercase();
            ["error", "failed", "unknown model", "exiting", "cannot"]
                .iter()
                .any(|needle| low.contains(needle))
        })
        .collect();
    if interesting.is_empty() {
        tail(lines)
    } else {
        tail(interesting)
    }
}
