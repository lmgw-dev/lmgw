//! The speech-to-text model as a setting (design §8, D5, WP7): what
//! Settings › Documents shows beside the choice, as the page-reading
//! model's row does (`model_help/settings.rs`): whether the audio stays on
//! this computer or goes to a named outside service, and which model
//! transcribes whenever the chosen one cannot run, and where that one is.
//! None is suggested and none is a default.

use lmgw_api_types::documents::model_help::{PageFallback, PageReader};

use crate::config::FallbackRoute;
use crate::extract::convert::model_help::settings::outside;
use crate::state::SharedState;

/// What Settings › Documents shows for the chosen speech-to-text model.
pub async fn facts(state: &SharedState) -> PageReader {
    let snap = state.snapshot();
    let alias = snap.settings.documents.asr_alias.trim().to_string();
    if alias.is_empty() {
        return PageReader::default();
    }
    let mut problems = Vec::new();
    let route = snap.resolve(&alias).ok();
    if route.is_none() && snap.candidate_alias(&alias).is_none() {
        problems.push(format!(
            "'{alias}' is not a model or alias lmgw serves (it was removed, renamed or disabled)."
        ));
    } else if let Some(task) = crate::capabilities::exposed::exposed_entry(state, &alias)
        .await
        .and_then(|e| e.capabilities)
        .map(|c| c.task)
        .filter(|t| t != "asr")
    {
        problems.push(format!(
            "'{alias}' is a '{task}' model; audio is transcribed by a speech-to-text model."
        ));
    }
    let target = route.as_ref().and_then(crate::vram::classify);
    let local = target.is_some() || snap.candidate_alias(&alias).is_some();
    let sent_to = if local {
        None
    } else {
        route.as_ref().and_then(outside)
    };
    // Whenever the chosen model cannot run, its configured fallback answers
    // (D14): the row says where that is before it happens.
    let fallback = match snap.hop_key(&alias).map(|key| snap.next_hop(&key)) {
        Some(FallbackRoute::Usable { alias, route, .. }) => {
            let outside = outside(&route);
            Some(PageFallback {
                alias,
                local: outside.is_none(),
                outside,
                sees: None,
            })
        }
        _ => None,
    };
    PageReader {
        alias,
        problems,
        local,
        outside: sent_to,
        fallback,
        ..Default::default()
    }
}
