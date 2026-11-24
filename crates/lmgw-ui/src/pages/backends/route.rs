//! Which entry the path selects: `/backends` (none), `/backends/builds/<id>`
//! (a build), `/backends/images` (every local image) and `/backends/windows`
//! (the pinned release builds). The last two are the addresses the Images
//! and Windows tabs always had, so links to them keep working.

/// What the detail pane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sel {
    Build(i64),
    Images,
    Windows,
}

impl Sel {
    /// The selection the route's `:tab?/:id?` name; `None` for none, and for
    /// a path that names nothing this page has (`/backends/builds`, an id
    /// that is no number), which then shows the list as it is.
    pub fn parse(tab: Option<&str>, id: Option<&str>) -> Option<Self> {
        match tab? {
            "images" => Some(Self::Images),
            "windows" => Some(Self::Windows),
            "builds" => id?.parse().ok().map(Self::Build),
            _ => None,
        }
    }

    /// The path that selects it.
    pub fn path(self) -> String {
        match self {
            Self::Build(id) => format!("/backends/builds/{id}"),
            Self::Images => "/backends/images".to_string(),
            Self::Windows => "/backends/windows".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_names_its_selection_and_back() {
        for sel in [Sel::Build(7), Sel::Images, Sel::Windows] {
            let path = sel.path();
            let mut parts = path.trim_start_matches("/backends/").split('/');
            assert_eq!(Sel::parse(parts.next(), parts.next()), Some(sel), "{path}");
        }
        // The tabs' old addresses are their entries'.
        assert_eq!(Sel::Images.path(), "/backends/images");
        assert_eq!(Sel::Windows.path(), "/backends/windows");
    }

    #[test]
    fn a_path_that_names_nothing_selects_nothing() {
        assert_eq!(Sel::parse(None, None), None);
        assert_eq!(Sel::parse(Some("builds"), None), None);
        assert_eq!(Sel::parse(Some("builds"), Some("x")), None);
        assert_eq!(Sel::parse(Some("nope"), Some("1")), None);
    }
}
