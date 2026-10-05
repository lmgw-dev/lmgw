//! The `realtime.semantic_vad` row a session runs (realtime design §6.3,
//! §12; WP6 review, fix package B6).
//!
//! A stored row that cannot run — a threshold or floor out of range, the
//! floor above the threshold, `realtime.semantic_floor_window_ms` past its
//! maximum wait (`SemanticVadRow::problem`) — is the owner's setting, not
//! the client's request: it used to make every client's `session.update` to
//! `semantic_vad` fail with `invalid_value`. A settings save refuses such a
//! row (`SemanticVadTable::problems`); one stored already — a hand edit, a
//! blob from an older build — is said at WARN when a session starts, and the
//! session runs the built-in row and floor window in its place.
//!
//! Only Smart Turn reads the threshold, floor and waits: with
//! `realtime.semantic_vad_engine` `server_vad` the stored row's
//! `silence_duration_ms` serves as it is.

use super::super::super::protocol::Eagerness;
use crate::config::{RealtimeSettings, SemanticVadEngine, SemanticVadRow, SemanticVadTable};

/// `table`'s row for `eagerness` (OpenAI's `auto` is `medium`).
fn of(table: &SemanticVadTable, eagerness: Option<Eagerness>) -> SemanticVadRow {
    match eagerness.unwrap_or(Eagerness::Auto) {
        Eagerness::High => table.high,
        Eagerness::Medium | Eagerness::Auto => table.medium,
        Eagerness::Low => table.low,
    }
}

/// The row and floor window a session runs for `eagerness` (module doc):
/// the stored ones, or the built-in ones when Smart Turn serves
/// `semantic_vad` and the stored row cannot run.
pub(super) fn row(
    eagerness: Option<Eagerness>,
    settings: &RealtimeSettings,
) -> (SemanticVadRow, u32) {
    let stored = of(&settings.semantic_vad, eagerness);
    let window = settings.semantic_floor_window_ms;
    if settings.semantic_vad_engine == SemanticVadEngine::SmartTurn
        && stored.problem(window).is_some()
    {
        let built_in = RealtimeSettings::default();
        return (
            of(&built_in.semantic_vad, eagerness),
            built_in.semantic_floor_window_ms,
        );
    }
    (stored, window)
}

/// At a session's start (module doc): one WARN naming every stored row
/// that cannot run; nothing when all can, or when Smart Turn does not serve
/// `semantic_vad`.
pub(crate) fn warn_unusable(session_id: &str, settings: &RealtimeSettings) {
    if settings.semantic_vad_engine != SemanticVadEngine::SmartTurn {
        return;
    }
    let problems = settings
        .semantic_vad
        .problems(settings.semantic_floor_window_ms);
    if problems.is_empty() {
        return;
    }
    tracing::warn!(
        "realtime {session_id}: {} — a semantic_vad session on such an eagerness runs the \
         built-in row and floor window instead; fix the setting",
        problems.join("; ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_that_cannot_run_is_replaced_by_the_built_in_one() {
        let built_in = RealtimeSettings::default();
        let mut s = RealtimeSettings::default();
        s.semantic_vad.high.floor = 0.9;
        s.semantic_vad.medium.threshold = 0.7;
        assert_eq!(
            row(Some(Eagerness::High), &s),
            (built_in.semantic_vad.high, 500)
        );
        // The others run as stored.
        assert_eq!(row(None, &s).0.threshold, 0.7);
        // A floor window past one row's wait: that row, with the built-in
        // window.
        s.semantic_floor_window_ms = 2500;
        assert_eq!(
            row(Some(Eagerness::Medium), &s),
            (s.semantic_vad.medium, 2500)
        );
        assert_eq!(
            row(Some(Eagerness::High), &s),
            (built_in.semantic_vad.high, 500)
        );
        // The server_vad engine reads only the stored silence window.
        s.semantic_vad_engine = SemanticVadEngine::ServerVad;
        assert_eq!(row(Some(Eagerness::High), &s), (s.semantic_vad.high, 2500));
    }
}
