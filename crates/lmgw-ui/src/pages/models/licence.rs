//! The licence chip on a Models row (decision-models design §1.5, §3.9): the
//! licence the row's GGUF header states, beside its kind. Information only,
//! so it has the plain badge look: no warning colour for any licence, and no
//! chip at all for a row whose header says nothing.

use leptos::prelude::*;
use lmgw_api_types::ModelLicence;

/// What the chip shows, flattened so a `Row` stays hashable.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct Chip {
    /// `general.license.name`, else `general.license`.
    label: String,
    /// The tooltip: the id the label did not show, and the link.
    title: String,
    /// The link, only when it is a web address the chip may open.
    href: Option<String>,
}

impl Chip {
    /// `None` when the header states no licence worth a label: a link alone
    /// has no name to put on a chip.
    pub(super) fn of(l: &ModelLicence) -> Option<Self> {
        let label = l.label()?.to_string();
        let mut title = format!("Licence stated by the GGUF header: {label}");
        if let (Some(id), Some(_)) = (&l.id, &l.name) {
            title.push_str(&format!(" (general.license: {id})"));
        }
        if let Some(link) = &l.link {
            title.push_str(&format!("\n{link}"));
        }
        let href = l
            .link
            .as_deref()
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            .map(str::to_string);
        Some(Self { label, title, href })
    }

    /// The words a filter box matches the row on.
    pub(super) fn haystack(&self) -> &str {
        &self.label
    }
}

#[component]
pub(super) fn LicenceChip(chip: Chip) -> impl IntoView {
    match chip.href {
        Some(href) => view! {
            <a
                class="type-badge licence-badge as-link"
                href=href
                target="_blank"
                rel="noreferrer"
                title=chip.title
            >
                {chip.label}
            </a>
        }
        .into_any(),
        None => view! {
            <span class="type-badge licence-badge" title=chip.title>
                {chip.label}
            </span>
        }
        .into_any(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lic(id: Option<&str>, name: Option<&str>, link: Option<&str>) -> ModelLicence {
        ModelLicence {
            id: id.map(Into::into),
            name: name.map(Into::into),
            link: link.map(Into::into),
        }
    }

    #[test]
    fn the_name_labels_the_chip_and_the_link_is_its_target() {
        let c = Chip::of(&lic(
            Some("other"),
            Some("Gemma Terms of Use"),
            Some("https://ai.google.dev/gemma/terms"),
        ))
        .unwrap();
        assert_eq!(c.label, "Gemma Terms of Use");
        assert_eq!(c.href.as_deref(), Some("https://ai.google.dev/gemma/terms"));
        assert!(c.title.contains("general.license: other"), "{}", c.title);
        assert!(c.title.contains("https://ai.google.dev/gemma/terms"));
    }

    #[test]
    fn the_id_labels_a_chip_without_a_name() {
        let c = Chip::of(&lic(Some("apache-2.0"), None, None)).unwrap();
        assert_eq!(c.label, "apache-2.0");
        assert_eq!(c.href, None);
    }

    #[test]
    fn a_link_that_is_not_a_web_address_is_only_a_tooltip() {
        let c = Chip::of(&lic(Some("mit"), None, Some("javascript:alert(1)"))).unwrap();
        assert_eq!(c.href, None);
        assert!(c.title.contains("javascript:alert(1)"));
    }

    #[test]
    fn a_licence_with_nothing_to_name_shows_no_chip() {
        assert!(Chip::of(&lic(None, None, Some("https://x.org/l"))).is_none());
    }

    #[test]
    fn no_licence_wording_is_special() {
        for id in ["cc-by-nc-4.0", "other", "unknown", "proprietary"] {
            let c = Chip::of(&lic(Some(id), None, None)).unwrap();
            assert_eq!(c.label, id, "the header's word, shown as it is");
        }
    }
}
