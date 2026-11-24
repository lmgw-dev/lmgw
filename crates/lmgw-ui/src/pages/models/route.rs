//! Which row the path selects: `/models` (none), `/models/<class>/<id>` (a
//! configured row), `/models/local/new` (a new local chat model, prefilled
//! by `?gguf=…&model_id=…`), `/models/upstream/<id>` (a passthrough
//! upstream's catalog line). `local` is the chat class's segment because
//! `/models/local/<id>` is the address the editor has always had, and other
//! pages link to it.

use super::Class;

/// What the detail pane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Sel {
    Row(Class, i64),
    /// A local chat model that is not saved yet.
    NewChat,
    /// A passthrough upstream, by its upstream id.
    Catalog(i64),
}

impl Class {
    /// The path segment its rows are selected by.
    pub(super) fn segment(self) -> &'static str {
        match self {
            Self::Chat => "local",
            Self::Aux => "aux",
            Self::Audio => "audio",
            Self::Image => "image",
            Self::Alias => "alias",
            Self::Candidate => "candidate",
        }
    }

    fn of_segment(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.segment() == s)
    }
}

impl Sel {
    /// The selection the route's `:class?/:id?` name; `None` for none, and
    /// for a path that names nothing this page has (an unknown class, an id
    /// that is no number), which then shows the list as it is.
    pub(super) fn parse(class: Option<&str>, id: Option<&str>) -> Option<Self> {
        let (class, id) = (class?, id?);
        if class == "local" && id == "new" {
            return Some(Self::NewChat);
        }
        let id: i64 = id.parse().ok()?;
        if class == "upstream" {
            return Some(Self::Catalog(id));
        }
        Class::of_segment(class).map(|c| Self::Row(c, id))
    }

    /// The path that selects it.
    pub(super) fn path(self) -> String {
        match self {
            Self::Row(c, id) => format!("/models/{}/{id}", c.segment()),
            Self::NewChat => "/models/local/new".to_string(),
            Self::Catalog(id) => format!("/models/upstream/{id}"),
        }
    }
}

/// The list's own query keys, carried from one selection to the next; a
/// new model's prefill (`gguf`, `model_id`) stays with the page it was
/// meant for.
pub(super) const LIST_KEYS: &[&str] = &["q", "show"];

/// The list's own query (`q`, `show`) out of a search string.
pub(super) fn list_query(search: &str) -> String {
    crate::widgets::list_pane::query_keeping(search, LIST_KEYS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_names_its_selection_and_back() {
        for sel in [
            Sel::Row(Class::Chat, 7),
            Sel::Row(Class::Aux, 2),
            Sel::Row(Class::Audio, 3),
            Sel::Row(Class::Image, 4),
            Sel::Row(Class::Alias, 5),
            Sel::Row(Class::Candidate, 6),
            Sel::NewChat,
            Sel::Catalog(9),
        ] {
            let path = sel.path();
            let mut parts = path.trim_start_matches("/models/").split('/');
            assert_eq!(Sel::parse(parts.next(), parts.next()), Some(sel), "{path}");
        }
        // The editor's old address is the chat row's.
        assert_eq!(Sel::Row(Class::Chat, 7).path(), "/models/local/7");
    }

    #[test]
    fn a_path_that_names_nothing_selects_nothing() {
        assert_eq!(Sel::parse(None, None), None);
        assert_eq!(Sel::parse(Some("local"), None), None);
        assert_eq!(Sel::parse(Some("local"), Some("x")), None);
        assert_eq!(Sel::parse(Some("nope"), Some("1")), None);
        assert_eq!(Sel::parse(Some("aux"), Some("new")), None);
    }

    #[test]
    fn the_list_query_travels_and_a_prefill_does_not() {
        assert_eq!(list_query("?q=qwen&show=chat"), "?q=qwen&show=chat");
        assert_eq!(list_query("?gguf=a.gguf&model_id=a&q=x"), "?q=x");
        assert_eq!(list_query(""), "");
        assert_eq!(list_query("?gguf=a.gguf"), "");
    }
}
