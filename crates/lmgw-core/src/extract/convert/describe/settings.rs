//! The picture-description model as a setting (design §4.4, §8, ruling 5,
//! D5, D14): what a save checks, what Settings › Documents shows beside the
//! choice (the page-reading model's facts, told for pictures:
//! [`facts_of`](crate::extract::convert::model_help::settings::facts_of)),
//! and the capability report's "Picture descriptions" row ([`apply`]), so
//! the page and a conversion agree. No model is suggested and none is a
//! default.

use lmgw_api_types::documents::model_help::PageReader;
use lmgw_api_types::documents::{DocCaps, FormatRow, Quality, Tier};

use crate::extract::convert::model_help::settings::{facts_of, validate_for};
use crate::extract::convert::model_help::Purpose;
use crate::state::SharedState;

/// The capability row's id ([`apply`]).
pub const ROW_ID: &str = "picture_descriptions";

/// The handler the row names when a model describes pictures.
pub const HANDLER: &str = "pictures.model";

/// A save's check of a non-empty `documents.picture_alias`.
pub async fn validate(state: &SharedState, alias: &str) -> Result<(), String> {
    validate_for(state, alias, Purpose::Pictures).await
}

/// What Settings › Documents shows for the chosen model.
pub async fn facts(state: &SharedState) -> PageReader {
    let snap = state.snapshot();
    let alias = snap.settings.documents.picture_alias.trim().to_string();
    facts_of(state, &snap, &alias, Purpose::Pictures).await
}

/// The "Picture descriptions" row (design §4.2's table) as the chosen model
/// makes it: described by the model, or kept without a description and
/// what to choose.
pub fn apply(caps: &mut DocCaps) {
    let r = &caps.picture_describer;
    let usable = !r.alias.is_empty() && r.problems.is_empty();
    let row = FormatRow {
        id: ROW_ID.to_string(),
        label: "Picture descriptions".to_string(),
        formats: Vec::new(),
        handler: usable.then(|| HANDLER.to_string()),
        tier: usable.then_some(Tier::Model),
        quality: if usable { Quality::Full } else { Quality::None },
        said: if usable {
            format!(
                "One short description per exported picture by {}, when a request asks for \
                 descriptions.",
                r.alias
            )
        } else if r.alias.is_empty() {
            "Pictures are kept without a description: choose a picture-description model \
             (\"Describe pictures with\")."
                .to_string()
        } else {
            // Each problem is a finished sentence of its own.
            format!(
                "Pictures are kept without a description: {}",
                r.problems.join(" ")
            )
        },
        needs: Vec::new(),
    };
    caps.formats.push(row);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::convert::report;
    use crate::extract::convert::tools::ToolFacts;
    use lmgw_api_types::documents::InstallPlatform;

    /// The row follows the choice, so the page and a conversion agree.
    #[test]
    fn the_picture_descriptions_row_says_who_describes() {
        let row = |r: PageReader| {
            let mut caps = report::doc_caps(&ToolFacts::all_missing(), InstallPlatform::Fedora);
            caps.picture_describer = r;
            apply(&mut caps);
            caps.formats.into_iter().find(|f| f.id == ROW_ID).unwrap()
        };
        let off = row(PageReader::default());
        assert_eq!((off.quality, off.handler.as_deref()), (Quality::None, None));
        assert!(off.said.contains("Describe pictures with"), "{}", off.said);
        let on = row(PageReader {
            alias: "example-vision".into(),
            ..Default::default()
        });
        assert_eq!(
            (on.quality, on.tier, on.handler.as_deref()),
            (Quality::Full, Some(Tier::Model), Some(HANDLER))
        );
        assert!(on.said.contains("by example-vision"), "{}", on.said);
        let gone = row(PageReader {
            alias: "example-gone".into(),
            problems: vec!["'example-gone' is not a model or alias lmgw serves".into()],
            ..Default::default()
        });
        assert_eq!(gone.quality, Quality::None);
        assert!(gone.said.contains("not a model or alias"), "{}", gone.said);
    }
}
