//! The list pane's rules, apart from the page they run in: where a key moves
//! the cursor, when the selected row is scrolled to, and which part of the
//! address a move carries along.

/// Where ↑ ↓ Home End go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Up,
    Down,
    First,
    Last,
}

/// What a key does in the list itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKey {
    Move(Step),
    /// Enter: open what the cursor stands on.
    Open,
}

/// The list's key, if it is one; none with Ctrl, Alt or Meta held (the
/// page's and the browser's keys).
pub fn list_key(key: &str, modified: bool) -> Option<ListKey> {
    if modified {
        return None;
    }
    Some(match key {
        "ArrowDown" => ListKey::Move(Step::Down),
        "ArrowUp" => ListKey::Move(Step::Up),
        "Home" => ListKey::Move(Step::First),
        "End" => ListKey::Move(Step::Last),
        "Enter" => ListKey::Open,
        _ => return None,
    })
}

/// What a key does in the search box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKey {
    /// Esc with text: empty the box.
    Clear,
    /// Esc in an empty box: back to the list.
    ToList,
    /// ↑ ↓ go on in the list.
    Move(Step),
    /// Enter: into the list, onto its first row when nothing is lit.
    Enter,
}

/// The search box's key, if it is one; `empty` is whether the box is.
pub fn search_key(key: &str, empty: bool) -> Option<SearchKey> {
    Some(match key {
        "Escape" if empty => SearchKey::ToList,
        "Escape" => SearchKey::Clear,
        "ArrowDown" => SearchKey::Move(Step::Down),
        "ArrowUp" => SearchKey::Move(Step::Up),
        "Enter" => SearchKey::Enter,
        _ => return None,
    })
}

/// The row a step lands on in a list of `len` rows, from row `from`: none
/// lit, ↓ lands on the first and ↑ on the last; the ends hold.
pub fn step_to(len: usize, from: Option<usize>, step: Step) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match (from, step) {
        (_, Step::First) | (None, Step::Down) => 0,
        (_, Step::Last) | (None, Step::Up) => last,
        (Some(i), Step::Up) => i.saturating_sub(1).min(last),
        (Some(i), Step::Down) => i.saturating_add(1).min(last),
    })
}

/// Where a move starts: in a narrow window the highlight (the cursor), else
/// the selection `sel`; in a wide one what is shown (`wide`, the selection
/// unless the page says otherwise), else the cursor. A wide window moves
/// from the selection because a move the unsaved-edits guard held back left
/// the cursor ahead of it.
pub fn origin<K>(narrow: bool, cursor: Option<K>, sel: Option<K>, wide: Option<K>) -> Option<K> {
    if narrow {
        cursor.or(sel)
    } else {
        wide.or(cursor)
    }
}

/// What the scroll to the selected row watches.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen<K> {
    pub sel: Option<K>,
    /// The list holds the selection's row.
    pub have: bool,
    pub narrow: bool,
}

/// Whether the selected row is scrolled to now: when the selection changed,
/// its row first appeared, or the list showed again in a window that turned
/// narrow or wide. Not on every change of the list: a live row (Traffic's
/// tail, a refreshed Models or Agents list) must not snap it back.
pub fn reveal_now<K: PartialEq>(prev: Option<&Seen<K>>, now: &Seen<K>) -> bool {
    now.have && now.sel.is_some() && prev != Some(now)
}

/// The part of a URL's query string (`?q=…&show=…`) whose keys `keep`
/// holds, in its order, with its `?`; empty when none.
pub fn query_keeping(search: &str, keep: &[&str]) -> String {
    query_where(search, |kv| {
        kv.split_once('=').is_some_and(|(k, _)| keep.contains(&k))
    })
}

/// The part of a URL's query string without the keys `drop` names: what a
/// selection carries along where the list's filters are all of the query
/// but the selection's own key.
pub fn query_without(search: &str, drop: &[&str]) -> String {
    query_where(search, |kv| {
        !drop.contains(&kv.split('=').next().unwrap_or(""))
    })
}

/// The pairs of a query string `keep` holds on to.
fn query_where(search: &str, keep: impl Fn(&str) -> bool) -> String {
    let kept: Vec<&str> = search
        .trim_start_matches('?')
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter(|kv| keep(kv))
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_lands_inside_the_list() {
        assert_eq!(step_to(0, None, Step::Down), None);
        assert_eq!(step_to(0, Some(2), Step::Last), None);
        assert_eq!(step_to(3, None, Step::Down), Some(0));
        assert_eq!(step_to(3, None, Step::Up), Some(2));
        assert_eq!(step_to(3, Some(0), Step::Up), Some(0));
        assert_eq!(step_to(3, Some(2), Step::Down), Some(2));
        assert_eq!(step_to(3, Some(1), Step::Down), Some(2));
        assert_eq!(step_to(3, Some(1), Step::Up), Some(0));
        assert_eq!(step_to(3, Some(1), Step::First), Some(0));
        assert_eq!(step_to(3, None, Step::Last), Some(2));
    }

    #[test]
    fn a_move_starts_from_the_highlight_when_narrow_and_the_selection_when_wide() {
        assert_eq!(origin(true, Some(1), Some(2), Some(2)), Some(1));
        assert_eq!(origin(true, None, Some(2), Some(2)), Some(2));
        assert_eq!(origin(false, Some(1), Some(2), Some(2)), Some(2));
        assert_eq!(origin(false, Some(1), None, None), Some(1));
        // Settings: the shown category, while the address names none.
        assert_eq!(origin(false, None, None, Some(5)), Some(5));
        assert_eq!(origin(true, None, None, Some(5)), None);
    }

    #[test]
    fn the_selected_row_is_scrolled_to_once_per_change() {
        let seen = |sel: Option<u8>, have, narrow| Seen { sel, have, narrow };
        let now = seen(Some(1), true, false);
        assert!(reveal_now(None, &now));
        assert!(!reveal_now(Some(&now), &now), "a live list leaves it");
        assert!(
            reveal_now(Some(&seen(Some(1), false, false)), &now),
            "its row appeared"
        );
        assert!(
            reveal_now(Some(&seen(Some(1), true, true)), &now),
            "the window turned wide"
        );
        assert!(reveal_now(Some(&seen(Some(2), true, false)), &now));
        assert!(
            !reveal_now(None, &seen(Some(1), false, false)),
            "no row to go to"
        );
        assert!(!reveal_now(None, &seen(None, false, false)));
    }

    #[test]
    fn keys_map_to_moves() {
        assert_eq!(
            list_key("ArrowDown", false),
            Some(ListKey::Move(Step::Down))
        );
        assert_eq!(list_key("End", false), Some(ListKey::Move(Step::Last)));
        assert_eq!(list_key("Enter", false), Some(ListKey::Open));
        assert_eq!(list_key("Home", true), None);
        assert_eq!(list_key("a", false), None);
        assert_eq!(search_key("Escape", true), Some(SearchKey::ToList));
        assert_eq!(search_key("Escape", false), Some(SearchKey::Clear));
        assert_eq!(
            search_key("ArrowUp", false),
            Some(SearchKey::Move(Step::Up))
        );
        assert_eq!(search_key("Enter", true), Some(SearchKey::Enter));
        assert_eq!(search_key("Home", false), None, "Home stays the box's");
    }

    #[test]
    fn the_query_keeps_the_lists_keys() {
        assert_eq!(
            query_keeping("?q=qwen&show=chat", &["q", "show"]),
            "?q=qwen&show=chat"
        );
        assert_eq!(
            query_keeping("?gguf=a.gguf&model_id=a&q=x", &["q", "show"]),
            "?q=x"
        );
        assert_eq!(query_keeping("?gguf=a.gguf", &["q"]), "");
        assert_eq!(query_keeping("", &["q"]), "");
        assert_eq!(
            query_keeping("?find=git&q=read&show=off&x=1", &["find", "q", "show"]),
            "?find=git&q=read&show=off"
        );
        assert_eq!(
            query_keeping("?qq=1&q=2", &["q"]),
            "?q=2",
            "a key is matched whole"
        );
    }

    #[test]
    fn the_query_drops_the_selections_key() {
        assert_eq!(query_without("?q=a&chain=x", &["chain"]), "?q=a");
        assert_eq!(query_without("?chain=x", &["chain"]), "");
        assert_eq!(query_without("", &["chain"]), "");
        assert_eq!(
            query_without("?alias=a&errors_only=true", &[]),
            "?alias=a&errors_only=true"
        );
    }
}
