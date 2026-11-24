//! The page-reading model as a setting (design §8, D5, D14, D15): what a
//! save checks, and what Settings › Documents shows beside the choice; the
//! picture-description model's row is told the same way ([`facts_of`],
//! `super::super::describe::settings`). No
//! model is suggested and none is a default: the page offers the vision
//! models that exist, and this module only says what is true of the one
//! chosen.

use lmgw_api_types::documents::model_help::{Outside, PageAnswer, PageFallback, PageReader};
use lmgw_api_types::documents::{DocCaps, Quality, Tier};

use super::Purpose;
use crate::config::{FallbackRoute, Route, Snapshot};
use crate::state::SharedState;

/// Where a route that is not one of lmgw's own local models sends: its
/// upstream's name and the host of its address (never the path, a query or
/// credentials). `None` for a local model.
pub(crate) fn outside(route: &Route) -> Option<Outside> {
    if crate::vram::classify(route).is_some() {
        return None;
    }
    Some(upstream_said(
        &route.upstream.name,
        &route.upstream.base_url,
    ))
}

/// An upstream as the row names it: its name, and its address's host.
fn upstream_said(name: &str, base_url: &str) -> Outside {
    let host = url::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    Outside {
        service: name.to_string(),
        host,
    }
}

/// Why `alias` cannot be the model for `purpose`, in the words a save
/// refuses with; `None` when it can. A model whose abilities lmgw does not
/// know is taken: unknown is not "cannot".
async fn problem(
    state: &SharedState,
    snap: &Snapshot,
    alias: &str,
    purpose: Purpose,
) -> Option<String> {
    if snap.candidate_alias(alias).is_none() && snap.resolve(alias).is_err() {
        return Some(format!(
            "'{alias}' is not a model or alias lmgw serves (it was removed, renamed or disabled)"
        ));
    }
    let caps = crate::capabilities::exposed::exposed_entry(state, alias)
        .await
        .and_then(|e| e.capabilities)?;
    let (done, cannot) = match purpose {
        Purpose::Pages => ("pages are read", "it cannot read a page's image"),
        Purpose::Pictures => ("pictures are described", "it cannot describe a picture"),
    };
    if caps.task != "chat" {
        return Some(format!(
            "'{alias}' is a '{}' model; {done} by a chat model that takes pictures",
            caps.task
        ));
    }
    (caps.vision == Some(false)).then(|| format!("'{alias}' does not take pictures, so {cannot}"))
}

/// A save's check of a non-empty `documents.page_reader`.
pub async fn validate(state: &SharedState, alias: &str) -> Result<(), String> {
    validate_for(state, alias, Purpose::Pages).await
}

/// A save's check of a non-empty choice for `purpose`.
pub(crate) async fn validate_for(
    state: &SharedState,
    alias: &str,
    purpose: Purpose,
) -> Result<(), String> {
    match problem(state, &state.snapshot(), alias, purpose).await {
        Some(why) => Err(format!("the {}: {why}", purpose.model())),
        None => Ok(()),
    }
}

/// llama-server's switch that keeps special tokens in the answer; a
/// DocTags model's tags are such tokens.
fn has_special(args: &[String]) -> bool {
    args.iter().any(|a| a == "--special" || a == "-sp")
}

/// What Settings › Documents shows for the chosen model.
pub async fn facts(state: &SharedState) -> PageReader {
    let snap = state.snapshot();
    let d = &snap.settings.documents;
    let alias = d.page_reader.trim().to_string();
    let answer = if d.page_reader_doctags {
        PageAnswer::Doctags
    } else {
        PageAnswer::Markdown
    };
    let mut r = facts_of(state, &snap, &alias, Purpose::Pages).await;
    r.answer = answer;
    if alias.is_empty() {
        return r;
    }
    let target = snap
        .resolve(&alias)
        .ok()
        .as_ref()
        .and_then(crate::vram::classify);
    let mut hints = Vec::new();
    if answer == PageAnswer::Doctags {
        let row = target
            .as_ref()
            .and_then(|t| snap.local_models.iter().find(|m| m.model_id == t.model_id));
        match row {
            Some(m) if !has_special(&m.args) => hints.push(format!(
                "'{}' is started without --special, so llama-server leaves the DocTags tags out \
                 of its answers and pages come back as plain text: add --special to the model's \
                 \"Extra llama-server flags\" under Models",
                m.model_id
            )),
            Some(_) => {}
            None => hints.push(
                "A DocTags model's llama-server must be started with --special, or its tags are \
                 left out of the answers; lmgw cannot see how this model's server is started"
                    .to_string(),
            ),
        }
    }
    r.hints = hints;
    r
}

/// What Settings › Documents shows for `alias` chosen for `purpose`: its
/// problems, whether it runs here or where it sends, and its fallback
/// (`answer` and `hints` left at their defaults). Empty `alias`: nothing.
pub(crate) async fn facts_of(
    state: &SharedState,
    snap: &Snapshot,
    alias: &str,
    purpose: Purpose,
) -> PageReader {
    if alias.is_empty() {
        return PageReader::default();
    }
    let alias = alias.to_string();
    let problems: Vec<String> = problem(state, snap, &alias, purpose)
        .await
        .map(|p| crate::extract::convert::report::sentence(&p))
        .into_iter()
        .collect();
    let route = snap.resolve(&alias).ok();
    let target = route.as_ref().and_then(crate::vram::classify);
    // A candidate alias picks among lmgw's own local models.
    let local = target.is_some() || snap.candidate_alias(&alias).is_some();
    let sent_to = if local {
        None
    } else {
        route.as_ref().and_then(outside)
    };
    // Whenever the chosen model cannot run (the GPU hold, a benchmark's
    // lease, a card too full, a climb that cannot happen, an upstream out
    // of quota or unreachable), its configured fallback answers (D14): the
    // page says where that is before it happens.
    let usable = snap.hop_key(&alias).map(|key| snap.next_hop(&key));
    let fallback = match usable {
        Some(FallbackRoute::Usable { alias, route, .. }) => {
            let sees = crate::capabilities::exposed::exposed_entry(state, &alias)
                .await
                .and_then(|e| e.capabilities)
                .and_then(|c| c.vision);
            let outside = outside(&route);
            Some(PageFallback {
                alias,
                local: outside.is_none(),
                outside,
                sees,
            })
        }
        _ => None,
    };
    PageReader {
        alias,
        answer: PageAnswer::Markdown,
        problems,
        local,
        outside: sent_to,
        fallback,
        hints: Vec::new(),
    }
}

/// The capability rows as the chosen model changes them, so the page and a
/// conversion agree: while nothing else reads scanned pages (the row has no
/// handler: tesseract is missing), a usable page-reading model reads a
/// PDF's, when the PDF itself can be read; where text recognition reads
/// them, the row adds that the model reads what it leaves or reads poorly
/// (`select`'s order). Picture files are not read by the model yet.
pub fn apply(caps: &mut DocCaps) {
    let r = &caps.page_reader;
    if r.alias.is_empty() || !r.problems.is_empty() {
        return;
    }
    let pdf_read = caps
        .formats
        .iter()
        .any(|f| f.id == "pdf" && f.handler.is_some());
    let alias = r.alias.clone();
    let Some(row) = caps
        .formats
        .iter_mut()
        .find(|f| f.id == "scans")
        .filter(|_| pdf_read)
    else {
        return;
    };
    if row.handler.is_none() {
        row.quality = Quality::Partial;
        row.tier = Some(Tier::Model);
        row.said = format!("Scanned PDF pages are read by {alias}, the page-reading model.");
    } else {
        row.said = format!(
            "{}; then {alias}, the page-reading model, reads the pages text recognition left \
             unread or read poorly.",
            row.said.trim_end_matches('.')
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scans row follows the chosen model only while nothing else reads
    /// scans, the PDF itself is read, and the choice has no problem.
    #[test]
    fn the_scans_row_says_who_reads_scanned_pages() {
        use crate::extract::convert::report;
        use crate::extract::convert::tools::ToolFacts;
        use lmgw_api_types::documents::InstallPlatform;
        let chosen = |problems: Vec<String>| PageReader {
            alias: "example-vision".into(),
            problems,
            ..Default::default()
        };
        // Without poppler the PDF is not read, so its scans are not either.
        let mut caps = report::doc_caps(&ToolFacts::all_missing(), InstallPlatform::Fedora);
        let before = caps.formats.clone();
        caps.page_reader = chosen(Vec::new());
        apply(&mut caps);
        assert_eq!(caps.formats, before);
        // With the PDF read: the row names the model.
        let pdf = caps.formats.iter_mut().find(|f| f.id == "pdf").unwrap();
        pdf.handler = Some("pdf.poppler".into());
        apply(&mut caps);
        let scans = caps.formats.iter().find(|f| f.id == "scans").unwrap();
        assert_eq!(
            (scans.quality, scans.tier),
            (Quality::Partial, Some(Tier::Model))
        );
        assert!(
            scans.said.contains("read by example-vision"),
            "{}",
            scans.said
        );
        // Where text recognition reads them, the model comes after it.
        let mut caps = report::doc_caps(&ToolFacts::all_missing(), InstallPlatform::Fedora);
        for f in caps
            .formats
            .iter_mut()
            .filter(|f| f.id == "pdf" || f.id == "scans")
        {
            f.handler = Some(format!("{}.example", f.id));
        }
        let before = caps
            .formats
            .iter()
            .find(|f| f.id == "scans")
            .unwrap()
            .clone();
        caps.page_reader = chosen(Vec::new());
        apply(&mut caps);
        let scans = caps.formats.iter().find(|f| f.id == "scans").unwrap();
        assert_eq!((scans.quality, scans.tier), (before.quality, before.tier));
        assert!(
            scans.said.starts_with(before.said.trim_end_matches('.'))
                && scans.said.contains(
                    "then example-vision, the page-reading model, reads the \
                     pages text recognition left unread or read poorly"
                ),
            "{}",
            scans.said
        );
        // A choice with a problem, or none, changes nothing.
        for reader in [chosen(vec!["gone".into()]), PageReader::default()] {
            let mut caps = report::doc_caps(&ToolFacts::all_missing(), InstallPlatform::Fedora);
            let pdf = caps.formats.iter_mut().find(|f| f.id == "pdf").unwrap();
            pdf.handler = Some("pdf.poppler".into());
            let before = caps.formats.clone();
            caps.page_reader = reader;
            apply(&mut caps);
            assert_eq!(caps.formats, before);
        }
    }

    /// S4: what the row says about a model outside lmgw is the upstream's
    /// name and its address's host, nothing else of the address.
    #[test]
    fn an_outside_model_is_named_by_its_upstream_and_host_only() {
        let o = upstream_said(
            "example-cloud",
            "https://user:secret@api.example:8443/v1?key=secret",
        );
        assert_eq!(
            (o.service.as_str(), o.host.as_str()),
            ("example-cloud", "api.example")
        );
        assert_eq!(upstream_said("example-cloud", "not a url").host, "");
        assert_eq!(
            upstream_said("example-box", "http://192.0.2.7:8080/v1").host,
            "192.0.2.7"
        );
    }

    #[test]
    fn the_special_switch_is_told_in_both_spellings() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(has_special(&args(&["--jinja", "--special"])));
        assert!(has_special(&args(&["-sp"])));
        assert!(!has_special(&args(&["--special-tokens", "--jinja"])));
    }
}
