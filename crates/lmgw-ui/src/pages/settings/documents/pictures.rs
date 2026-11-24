//! Settings › Documents' "Describe pictures with" (document-conversion
//! design §5.3, §8, ruling 5, D5, D14): the picture-description model, off
//! until one is chosen, beside "Read pages with a model" in Model help and
//! told the same way. The chooser is the shared model picker over the chat
//! models that take pictures; none is suggested and none is a default. It
//! may be the same model as the page-reading one. Beside the choice the row
//! states, before it happens, whether the chosen model runs on this
//! computer or pictures are sent to a named outside service
//! ([`where_said`]), and which model answers whenever the chosen one cannot
//! run, where that one is, and whether it can see a picture at all
//! ([`fallback_said`]).
//!
//! Saved at once through the dashboard's `settings_set`
//! (`documents_picture_alias`). The admin tools cannot set it: choosing
//! where document pictures are sent is the owner's own act.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::model_help::PageReader;
use serde_json::json;

use super::model::{cannot_see, outside_said};
use super::use_docs;
use crate::widgets::{use_toasts, ModelPicker};

/// Where the chosen model runs: on this computer, or which outside service
/// pictures are sent to. `None` while none is chosen.
pub(super) fn where_said(r: &PageReader) -> Option<String> {
    if r.alias.is_empty() {
        return None;
    }
    Some(match (&r.outside, r.local) {
        (Some(o), _) => format!(
            "{} is not a model lmgw runs on this computer: the pictures it describes are sent \
             to {}.",
            r.alias,
            outside_said(o)
        ),
        (None, true) => format!(
            "{} runs on this computer: pictures described by it stay here.",
            r.alias
        ),
        (None, false) => format!(
            "lmgw cannot tell where {} runs (its name does not resolve to a model now).",
            r.alias
        ),
    })
}

/// What happens whenever the chosen model cannot run, in plain words: who
/// answers in its place and where that is. `None` while none is chosen.
pub(super) fn fallback_said(r: &PageReader) -> Option<String> {
    if r.alias.is_empty() {
        return None;
    }
    let when = if r.local {
        "cannot run (the GPU hold is on, the graphics card is full or in use by a benchmark)"
    } else {
        "cannot answer (its service is out of quota or cannot be reached)"
    };
    let Some(f) = &r.fallback else {
        return Some(format!(
            "Whenever {} {when}, pictures are kept without a description, and the answer says \
             which ({} has no fallback).",
            r.alias, r.alias
        ));
    };
    if f.sees == Some(false) {
        return Some(format!(
            "Whenever {} {when}, its fallback {} answers in its place, but {} takes no \
             pictures: nothing is sent to it, pictures are kept without a description, and the \
             answer says which.",
            r.alias, f.alias, f.alias
        ));
    }
    let to = match &f.outside {
        Some(o) => format!(
            "{} is not a model lmgw runs on this computer, so then the pictures are sent to {}",
            f.alias,
            outside_said(o)
        ),
        None if r.local => format!("{} runs on this computer too", f.alias),
        None => format!("{} runs on this computer", f.alias),
    };
    Some(format!(
        "Whenever {} {when}, pictures go to its fallback {} instead: {to}.",
        r.alias, f.alias
    ))
}

/// What the row says under its title.
pub(super) fn sub_said(r: &PageReader) -> String {
    if r.alias.is_empty() {
        "Off. A request that asks for picture descriptions gets its pictures without one; \
         nothing is sent to a model until you choose one."
            .to_string()
    } else {
        format!(
            "One short description per picture, by {}, for the requests that ask for \
             descriptions (docling callers ask with do_picture_description); other requests \
             send it nothing.",
            r.alias
        )
    }
}

#[component]
pub(super) fn PictureRow(describer: PageReader) -> impl IntoView {
    let d = use_docs();
    let toasts = use_toasts();
    let saved = StoredValue::new(describer.alias.clone());
    let value = RwSignal::new(describer.alias.clone());
    let saving = RwSignal::new(false);
    Effect::new(move |_| {
        let picked = value.get();
        if picked == saved.get_value() {
            return;
        }
        saved.set_value(picked.clone());
        let done = if picked.is_empty() {
            "Pictures are not described".to_string()
        } else {
            format!("Pictures are described by {picked}")
        };
        saving.set(true);
        spawn_local(async move {
            let body = json!({ "documents_picture_alias": picked });
            match crate::api::post::<serde_json::Value, _>("/api/op/settings_set", &body).await {
                Ok(_) => toasts.ok(done),
                Err(e) => toasts.err(e.to_string()),
            }
            saving.try_set(false);
            // The row is drawn again from what is stored now, a refused
            // choice included.
            d.load(false);
        });
    });
    let goes = where_said(&describer);
    let leaves = describer.outside.is_some();
    let hold = fallback_said(&describer);
    let problems = describer.problems.clone();
    view! {
        <div class="prow doc-row stack" data-picture-describer="">
            <div class="prow-text">
                <span class="prow-title">"Describe pictures with"</span>
                <span class="prow-sub">{sub_said(&describer)}</span>
                {goes.map(|g| {
                    view! { <span class="prow-sub" class:doc-warn=leaves data-picture-describer-where="">{g}</span> }
                })}
                {hold.map(|h| view! { <span class="prow-sub" data-picture-describer-fallback="">{h}</span> })}
                {problems
                    .into_iter()
                    .map(|p| view! { <span class="prow-sub doc-warn">{p}</span> })
                    .collect_view()}
            </div>
            <div class="prow-end">
                <ModelPicker
                    value=value
                    tasks=&["chat"]
                    empty_label="Off"
                    disallow=(Callback::new(cannot_see), "does not take pictures")
                    recent_key="documents.picture_alias"
                    disabled=saving
                />
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::model_help::{Outside, PageFallback};

    fn outside(service: &str, host: &str) -> Option<Outside> {
        Some(Outside {
            service: service.into(),
            host: host.into(),
        })
    }

    #[test]
    fn the_row_says_whether_pictures_stay_on_this_computer() {
        assert_eq!(where_said(&PageReader::default()), None);
        let here = where_said(&PageReader {
            alias: "example-vision".into(),
            local: true,
            ..Default::default()
        })
        .unwrap();
        assert!(
            here.contains("pictures described by it stay here"),
            "{here}"
        );
        let away = where_said(&PageReader {
            alias: "example-cloud/vision".into(),
            outside: outside("example-cloud", "api.example"),
            ..Default::default()
        })
        .unwrap();
        assert!(
            away.contains("sent to the service \"example-cloud\" (api.example)"),
            "{away}"
        );
    }

    #[test]
    fn the_row_says_who_describes_whenever_the_model_cannot_run() {
        let mut r = PageReader {
            alias: "example-vision".into(),
            local: true,
            ..Default::default()
        };
        let none = fallback_said(&r).unwrap();
        assert!(none.contains("kept without a description"), "{none}");
        assert!(none.contains("the GPU hold is on"), "{none}");
        r.fallback = Some(PageFallback {
            alias: "example-cloud/vision".into(),
            local: false,
            outside: outside("example-cloud", "api.example"),
            sees: Some(true),
        });
        let away = fallback_said(&r).unwrap();
        assert!(
            away.contains("pictures go to its fallback example-cloud/vision instead"),
            "{away}"
        );
        assert!(away.contains("(api.example)"), "{away}");
        r.fallback = Some(PageFallback {
            alias: "example-text".into(),
            local: false,
            outside: outside("example-cloud", "api.example"),
            sees: Some(false),
        });
        let blind = fallback_said(&r).unwrap();
        assert!(
            blind.contains("takes no pictures: nothing is sent to it"),
            "{blind}"
        );
        assert!(sub_said(&PageReader::default()).starts_with("Off."));
        assert!(sub_said(&r).contains("by example-vision"));
    }
}
