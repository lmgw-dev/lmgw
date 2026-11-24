//! The page's answer (document-conversion design §4.4, §8): [`DocCaps`]
//! rendered from the table ([`super::caps`]) and the snapshot
//! ([`super::tools`]), with [`super::caps::resolve`] for every format: the
//! one place the page's rows come from.

use lmgw_api_types::documents::{
    DocCaps, FormatRow, HintKind, InstallHint, InstallPlatform, MissingRow, Quality, ToolRow,
    ToolState,
};

use super::caps::{self, Format, Install, ToolId, GROUPS, HANDLERS, TOOLS};
use super::tools::{Fact, ToolFacts};

pub use super::platform::platform;

/// A sentence for the page: its first letter up and a full stop at its end,
/// so a row's words arrive finished. A text that starts with a quoted name
/// keeps it as it is.
pub(crate) fn sentence(s: &str) -> String {
    let mut c = s.chars();
    let mut out: String = match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => return String::new(),
    };
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

/// The image the server's unit runs (docs/server.md §3), what a derived
/// image starts `FROM`.
const SERVER_IMAGE: &str = "localhost/lmgw-server:current";

/// The labels of the page rows a handler needing `id` reads.
fn group_labels_needing(id: ToolId) -> Vec<String> {
    GROUPS
        .iter()
        .filter(|g| {
            g.formats.iter().any(|f| {
                HANDLERS
                    .iter()
                    .any(|h| h.formats.contains(f) && h.needs.contains(&id))
            })
        })
        .map(|g| g.label.to_string())
        .collect()
}

pub(super) fn hint(tool: &caps::Tool, platform: InstallPlatform) -> InstallHint {
    let (install, note) = caps::install_hint(&tool.packages, platform);
    let note = (!note.is_empty()).then(|| note.to_string());
    let (kind, text) = match (install, platform) {
        (Install::Command(pkgs), InstallPlatform::Fedora) => {
            (HintKind::Command, format!("sudo dnf install {pkgs}"))
        }
        // A `dnf` in the container would not survive it being recreated
        // (design §6.2): a derived image instead.
        (Install::Command(pkgs), InstallPlatform::ServerImage) => (
            HintKind::Command,
            format!(
                "FROM {SERVER_IMAGE}\nRUN dnf -y install --setopt=install_weak_deps=False \
                 {pkgs} && dnf clean all"
            ),
        ),
        (Install::Command(c), _) => (HintKind::Command, c.to_string()),
        (Install::Manual(name), _) => (
            HintKind::Manual,
            format!("Install {name} with your distribution's package manager."),
        ),
        (Install::Link(url), _) => (HintKind::Link, url.to_string()),
        (Install::NotYet(why), _) => (HintKind::NotYet, why.to_string()),
        (Install::Bundled, InstallPlatform::ServerImage) => (
            HintKind::Manual,
            format!(
                "{} is part of the lmgw server image and was not found beside lmgw-server: \
                 build or pull the image again.",
                tool.label
            ),
        ),
        (Install::Bundled, _) => (
            HintKind::Manual,
            format!(
                "{} is installed with lmgw and was not found beside it: install lmgw again.",
                tool.label
            ),
        ),
    };
    InstallHint {
        platform,
        kind,
        text,
        note,
    }
}

fn tool_row(tool: &caps::Tool, facts: &ToolFacts) -> ToolRow {
    let t = facts.tools.get(&tool.id);
    let (state, version, path, extras, message) = match t.map(|t| &t.fact) {
        Some(Fact::Found(f)) => (
            ToolState::Found,
            Some(f.version.clone()),
            Some(f.located.path.display().to_string()),
            f.extras.clone(),
            None,
        ),
        Some(Fact::ProbeFailed { located, message }) => (
            ToolState::ProbeFailed,
            None,
            Some(located.path.display().to_string()),
            Vec::new(),
            Some(message.clone()),
        ),
        Some(Fact::Missing) | None => (ToolState::Missing, None, None, Vec::new(), None),
    };
    ToolRow {
        id: tool.id.as_str().to_string(),
        label: tool.label.to_string(),
        state,
        version,
        path,
        extras,
        message,
        licence: tool.licence.to_string(),
        used_for: group_labels_needing(tool.id),
        checked_at: t.map_or(facts.checked_at, |t| t.checked_at),
        not_confined: super::confine::verdict::state(tool.id, facts),
    }
}

/// What the page says when the preferred handler's tools are not usable
/// here: the probe that failed, why there is nothing to install yet, or
/// what to install.
pub(super) fn needs_said(
    missing: &[ToolId],
    facts: &ToolFacts,
    platform: InstallPlatform,
) -> String {
    let names: Vec<&str> = missing.iter().map(|t| caps::tool(*t).label).collect();
    // LibreOffice where it cannot be confined: why, not an install command.
    let unconfined = missing.iter().find_map(|t| match facts.fact(*t) {
        Some(Fact::ProbeFailed { message, .. })
            if message.starts_with(super::confine::NEEDS_LANDLOCK) =>
        {
            Some(message.clone())
        }
        _ => None,
    });
    if let Some(why) = unconfined {
        return format!("not read: {why}");
    }
    let failed = missing
        .iter()
        .any(|t| matches!(facts.fact(*t), Some(Fact::ProbeFailed { .. })));
    if failed {
        return format!(
            "not read: {} did not answer its check; see below",
            names.join(" and ")
        );
    }
    let mut install = Vec::new();
    for t in missing {
        let tool = caps::tool(*t);
        match caps::install_hint(&tool.packages, platform).0 {
            Install::NotYet(why) => return why.to_string(),
            Install::Bundled => {
                return format!(
                    "not read: {}, which is installed with lmgw, was not found; install lmgw \
                     again",
                    tool.label
                )
            }
            Install::Manual(name) => install.push(name),
            _ if matches!(
                platform,
                InstallPlatform::Fedora | InstallPlatform::ServerImage
            ) =>
            {
                install.push(tool.packages.fedora)
            }
            _ => install.push(tool.label),
        }
    }
    format!("not read: install {}", install.join(" and "))
}

fn format_row(g: &caps::Group, facts: &ToolFacts, platform: InstallPlatform) -> FormatRow {
    // The formats of a row resolve alike (`caps::tests::the_table_is_whole`),
    // so the first one speaks for the row.
    let first: Format = g.formats[0];
    let r = caps::resolve(first, facts);
    let (quality, said) = match (r.handler, r.no_reader) {
        // A lesser row read it, its words say what the passed-over tools add.
        (Some(h), _) if !r.passed_over.is_empty() => (Quality::Partial, h.gives.to_string()),
        (Some(h), _) => (Quality::Full, h.gives.to_string()),
        (None, Some(why)) => (Quality::None, why.to_string()),
        (None, None) => (Quality::None, needs_said(&r.missing, facts, platform)),
    };
    FormatRow {
        id: g.id.to_string(),
        label: g.label.to_string(),
        formats: g.formats.iter().map(|f| f.as_str().to_string()).collect(),
        handler: r.handler.map(|h| h.id.to_string()),
        tier: r.handler.map(|h| h.tier),
        quality,
        said: sentence(&said),
        needs: r
            .missing
            .iter()
            .chain(&r.passed_over)
            .map(|t| t.as_str().to_string())
            .collect(),
    }
}

/// The whole answer, for `platform`.
pub fn doc_caps(facts: &ToolFacts, platform: InstallPlatform) -> DocCaps {
    let missing = TOOLS
        .iter()
        .filter(|t| matches!(facts.fact(t.id), Some(Fact::Missing) | None))
        .map(|t| MissingRow {
            tool: t.id.as_str().to_string(),
            label: t.label.to_string(),
            install: vec![hint(t, platform)],
            gives: group_labels_needing(t.id),
            licence: t.licence.to_string(),
        })
        .chain(super::ocr::langs::osd_missing(facts, platform))
        .collect();
    DocCaps {
        tools: TOOLS.iter().map(|t| tool_row(t, facts)).collect(),
        formats: GROUPS
            .iter()
            .map(|g| format_row(g, facts, platform))
            .collect(),
        missing,
        checked_at: facts.checked_at,
        platform,
        pool: super::pool::global().facts(),
        web: Default::default(),
        // The model files need the download queue's rows: `super::doc_tools`.
        models: Vec::new(),
        // The op fills it from the settings (`model_help::settings::facts`).
        page_reader: Default::default(),
        // And the picture-description model (`describe::settings::facts`).
        picture_describer: Default::default(),
        speech: Default::default(),
        confinement: super::confine::verdict::confinement(facts),
        answers: super::budget::answers::global().facts(),
        // The op fills it from the settings (`jobs::quota::facts`).
        job_bounds: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_poppler_the_pdf_row_names_the_package_and_poppler_is_missing() {
        // Without lmgw's reader no tool starts: that one state on the row.
        let none = doc_caps(&ToolFacts::all_missing(), InstallPlatform::Fedora);
        let pdf = none.formats.iter().find(|f| f.id == "pdf").unwrap();
        assert!(pdf.said.contains("lmgw's document reader"), "{}", pdf.said);
        assert_eq!(pdf.needs, vec!["poppler", "reader"]);
        let caps = doc_caps(
            &ToolFacts::all_missing().with_found(ToolId::Reader),
            InstallPlatform::Fedora,
        );
        let pdf = caps.formats.iter().find(|f| f.id == "pdf").unwrap();
        assert_eq!(pdf.quality, Quality::None);
        assert_eq!(pdf.said, "Not read: install poppler-utils.");
        assert_eq!(pdf.needs, vec!["poppler"]);
        let poppler = caps.missing.iter().find(|m| m.tool == "poppler").unwrap();
        assert_eq!(poppler.gives, vec!["PDF"]);
        assert_eq!(poppler.install[0].text, "sudo dnf install poppler-utils");
        let ts = caps.missing.iter().find(|m| m.tool == "tesseract").unwrap();
        assert_eq!(
            ts.gives,
            vec![
                "Scanned PDF pages",
                "Pictures of text (PNG, JPEG, GIF, WebP, TIFF)"
            ]
        );
        // lmgw's own reader is a row like any tool: missing here, with the
        // formats it reads and nothing to install but lmgw itself.
        let reader = none.missing.iter().find(|m| m.tool == "reader").unwrap();
        assert_eq!(reader.label, "lmgw's document reader");
        assert!(reader.gives.contains(&"Web pages (HTML)".to_string()));
        assert_eq!(reader.install[0].kind, HintKind::Manual);
        assert!(
            reader.install[0].text.contains("install lmgw again"),
            "{}",
            reader.install[0].text
        );
        let html = none.formats.iter().find(|f| f.id == "html").unwrap();
        if cfg!(unix) {
            assert_eq!(html.quality, Quality::None);
            assert_eq!(html.needs, vec!["reader"]);
            assert!(html.said.contains("install lmgw again"), "{}", html.said);
        }
        let office = caps.formats.iter().find(|f| f.id == "office").unwrap();
        assert_eq!(office.quality, Quality::Full);
        assert_eq!(office.handler.as_deref(), Some("office.native"));
    }

    #[test]
    fn with_poppler_pdfs_are_read_and_it_is_listed_as_found() {
        let facts = ToolFacts::all_missing()
            .with_found(ToolId::Poppler)
            .with_found(ToolId::Reader);
        let caps = doc_caps(&facts, InstallPlatform::Fedora);
        let pdf = caps.formats.iter().find(|f| f.id == "pdf").unwrap();
        assert_eq!(pdf.quality, Quality::Full);
        assert_eq!(pdf.handler.as_deref(), Some("pdf.poppler"));
        assert!(caps.missing.iter().all(|m| m.tool != "poppler"));
        let row = caps.tools.iter().find(|t| t.id == "poppler").unwrap();
        assert_eq!(row.state, ToolState::Found);
        assert_eq!(row.version.as_deref(), Some("0.0.0-example"));
        assert_eq!(row.used_for, vec!["PDF"]);
    }

    /// tesseract found without its orientation data: the page lists the
    /// package with what it adds; with tesseract missing, only tesseract.
    #[test]
    fn tesseract_without_orientation_data_lists_tesseract_osd() {
        let facts = ToolFacts::all_missing()
            .with_found(ToolId::Tesseract)
            .with_found(ToolId::Reader);
        let caps = doc_caps(&facts, InstallPlatform::Fedora);
        let osd = caps
            .missing
            .iter()
            .find(|m| m.tool == "tesseract-osd")
            .unwrap();
        assert_eq!(osd.gives, vec!["rotated scans"]);
        assert_eq!(osd.install[0].text, "sudo dnf install tesseract-osd");
        let scans = caps.formats.iter().find(|f| f.id == "scans").unwrap();
        assert_eq!(scans.handler.as_deref(), Some("ocr.tesseract"));
        let none = doc_caps(&ToolFacts::all_missing(), InstallPlatform::Fedora);
        assert!(none.missing.iter().all(|m| m.tool != "tesseract-osd"));
    }

    #[test]
    fn another_linux_is_told_the_tool_and_given_nothing_to_copy() {
        let caps = doc_caps(
            &ToolFacts::all_missing().with_found(ToolId::Reader),
            InstallPlatform::OtherLinux,
        );
        let poppler = caps.missing.iter().find(|m| m.tool == "poppler").unwrap();
        let h = &poppler.install[0];
        assert_eq!(h.platform, InstallPlatform::OtherLinux);
        assert_eq!(h.kind, HintKind::Manual);
        assert_eq!(
            h.text,
            "Install poppler with your distribution's package manager."
        );
        assert_eq!(h.note, None);
        let pdf = caps.formats.iter().find(|f| f.id == "pdf").unwrap();
        assert_eq!(pdf.said, "Not read: install poppler.");
    }

    #[test]
    fn the_server_image_says_to_derive_an_image() {
        let caps = doc_caps(&ToolFacts::all_missing(), InstallPlatform::ServerImage);
        let pandoc = caps.missing.iter().find(|m| m.tool == "pandoc").unwrap();
        let h = &pandoc.install[0];
        assert_eq!(h.platform, InstallPlatform::ServerImage);
        assert!(
            h.text.starts_with("FROM localhost/lmgw-server:current\n"),
            "{}",
            h.text
        );
        assert!(h.text.contains("dnf -y install"), "{}", h.text);
        assert!(h.text.contains("pandoc-cli"), "{}", h.text);
    }
}
