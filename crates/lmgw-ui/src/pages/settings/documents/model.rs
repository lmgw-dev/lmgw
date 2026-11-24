//! Settings › Documents' "Read pages with a model" (document-conversion
//! design §8, D5, D14, D15): the page-reading model, off until one is
//! chosen. The chooser is the shared model picker over the chat models
//! that take pictures; none is suggested and none is a default. Beside the
//! choice the row states what the gateway knows about where pages go, in
//! plain words and before it happens: whether the chosen model runs on
//! this computer or pages are sent to a named outside service
//! ([`where_said`]), and which model answers whenever the chosen one cannot
//! run, where that one is, and whether it can read a page at all
//! ([`fallback_said`]). The expert view adds the answer format (DocTags,
//! for a docling model) and what such a model's row still needs.
//!
//! Saved at once through the dashboard's `settings_set`
//! (`documents_page_reader`, `documents_page_reader_doctags`), like
//! "Parallel tool runs". The admin tools cannot set either: choosing where
//! document pages are sent is the owner's own act.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::model_help::{Outside, PageAnswer, PageReader};
use serde_json::json;

use super::use_docs;
use crate::catalog::CatalogEntry;
use crate::widgets::{use_toasts, ModelPicker, Switch};

/// A model that says it cannot see is not offered for reading pages.
pub(super) fn cannot_see(e: CatalogEntry) -> bool {
    e.vision == Some(false)
}

/// A service outside lmgw, as the row names it.
pub(super) fn outside_said(o: &Outside) -> String {
    if o.host.is_empty() {
        format!("the service \"{}\"", o.service)
    } else {
        format!("the service \"{}\" ({})", o.service, o.host)
    }
}

/// Where the chosen model runs: on this computer, or which outside service
/// pages are sent to. `None` while none is chosen.
pub(super) fn where_said(r: &PageReader) -> Option<String> {
    if r.alias.is_empty() {
        return None;
    }
    Some(match (&r.outside, r.local) {
        (Some(o), _) => format!(
            "{} is not a model lmgw runs on this computer: these pages are sent to {}.",
            r.alias,
            outside_said(o)
        ),
        (None, true) => format!(
            "{} runs on this computer: pages read by it stay here.",
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
    // When a model cannot run: a local one and one outside differ.
    let when = if r.local {
        "cannot run (the GPU hold is on, the graphics card is full or in use by a benchmark)"
    } else {
        "cannot answer (its service is out of quota or cannot be reached)"
    };
    let Some(f) = &r.fallback else {
        return Some(format!(
            "Whenever {} {when}, no model reads pages: they keep the text the tools read, and \
             the answer says which pages ({} has no fallback).",
            r.alias, r.alias
        ));
    };
    if f.sees == Some(false) {
        return Some(format!(
            "Whenever {} {when}, its fallback {} answers in its place, but {} takes no \
             pictures and cannot read a page: nothing is sent to it, pages keep the text the \
             tools read, and the answer says which pages.",
            r.alias, f.alias, f.alias
        ));
    }
    let to = match &f.outside {
        Some(o) => format!(
            "{} is not a model lmgw runs on this computer, so then these pages are sent to {}",
            f.alias,
            outside_said(o)
        ),
        None if r.local => format!("{} runs on this computer too", f.alias),
        None => format!("{} runs on this computer", f.alias),
    };
    Some(format!(
        "Whenever {} {when}, pages go to its fallback {} instead: {to}.",
        r.alias, f.alias
    ))
}

/// What the row says under its title.
pub(super) fn sub_said(r: &PageReader) -> String {
    if r.alias.is_empty() {
        "Off. Pages that neither their text layer nor text recognition read well, and pages \
         with tables, can be read by a model that takes pictures; nothing is sent to a model \
         until you choose one."
            .to_string()
    } else {
        format!(
            "Pages that neither their text layer nor text recognition read well, and pages \
             with tables, are read by {}, also for a request that says nothing about it; a \
             request can turn it off, keep it to pages nothing else read, or ask for every \
             page.",
            r.alias
        )
    }
}

#[component]
pub(super) fn ModelRow(reader: PageReader) -> impl IntoView {
    let d = use_docs();
    let toasts = use_toasts();
    let saved = StoredValue::new(reader.alias.clone());
    let value = RwSignal::new(reader.alias.clone());
    let doctags = RwSignal::new(reader.answer == PageAnswer::Doctags);
    let was_doctags = reader.answer == PageAnswer::Doctags;
    let saving = RwSignal::new(false);
    let save = move |body: serde_json::Value, done: String| {
        saving.set(true);
        spawn_local(async move {
            match crate::api::post::<serde_json::Value, _>("/api/op/settings_set", &body).await {
                Ok(_) => toasts.ok(done),
                Err(e) => toasts.err(e.to_string()),
            }
            saving.try_set(false);
            // The row is drawn again from what is stored now, a refused
            // choice included.
            d.load(false);
        });
    };
    Effect::new(move |_| {
        let picked = value.get();
        if picked == saved.get_value() {
            return;
        }
        saved.set_value(picked.clone());
        let done = if picked.is_empty() {
            "Pages are read without a model".to_string()
        } else {
            format!("Pages are read with {picked}")
        };
        save(json!({ "documents_page_reader": picked }), done);
    });
    Effect::new(move |_| {
        let on = doctags.get();
        if on == was_doctags {
            return;
        }
        let done = if on {
            "The model's answers are read as DocTags".to_string()
        } else {
            "The model's answers are read as Markdown".to_string()
        };
        save(json!({ "documents_page_reader_doctags": on }), done);
    });
    let chosen = !reader.alias.is_empty();
    let goes = where_said(&reader);
    let leaves = reader.outside.is_some();
    let hold = fallback_said(&reader);
    let problems = reader.problems.clone();
    let hints = reader.hints.clone();
    let expert = d.expert;
    view! {
        <div class="prow doc-row stack" data-page-reader="">
            <div class="prow-text">
                <span class="prow-title">"Read pages with a model"</span>
                <span class="prow-sub">{sub_said(&reader)}</span>
                {goes.map(|g| {
                    view! { <span class="prow-sub" class:doc-warn=leaves data-page-reader-where="">{g}</span> }
                })}
                {hold.map(|h| view! { <span class="prow-sub" data-page-reader-fallback="">{h}</span> })}
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
                    recent_key="documents.page_reader"
                    disabled=saving
                />
            </div>
            <div class="prow-extra" hidden=move || !expert.get() || !chosen>
                <label class="doc-expert-switch">
                    <Switch value=doctags label="Answers in DocTags" disabled=saving/>
                    <span>
                        "Answers in DocTags (a docling model's page format, read with positions \
                         and tables; other models answer in Markdown)"
                    </span>
                </label>
                {hints
                    .into_iter()
                    .map(|h| view! { <div class="prow-sub doc-warn">{h}</div> })
                    .collect_view()}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::model_help::PageFallback;

    fn outside(service: &str, host: &str) -> Option<Outside> {
        Some(Outside {
            service: service.into(),
            host: host.into(),
        })
    }

    fn local(alias: &str) -> PageReader {
        PageReader {
            alias: alias.into(),
            local: true,
            ..Default::default()
        }
    }

    fn cloud(alias: &str) -> PageReader {
        PageReader {
            alias: alias.into(),
            outside: outside("example-cloud", "api.example"),
            ..Default::default()
        }
    }

    /// S4 (a): a model outside this computer says so, with the service.
    #[test]
    fn the_row_says_whether_pages_stay_on_this_computer() {
        assert_eq!(where_said(&PageReader::default()), None);
        let here = where_said(&local("example-vision")).unwrap();
        assert!(here.contains("runs on this computer"), "{here}");
        let away = where_said(&cloud("example-cloud/vision")).unwrap();
        assert!(
            away.contains("these pages are sent to the service \"example-cloud\" (api.example)"),
            "{away}"
        );
        assert!(
            away.contains("not a model lmgw runs on this computer"),
            "{away}"
        );
    }

    /// S4 (b), (c): the fallback is named with where it is, and it answers
    /// whenever the model cannot run, not only under the hold.
    #[test]
    fn the_row_says_who_answers_whenever_the_model_cannot_run_and_where() {
        assert_eq!(fallback_said(&PageReader::default()), None);
        let mut r = local("example-vision");
        let none = fallback_said(&r).unwrap();
        assert!(none.contains("no model reads pages"), "{none}");
        assert!(
            none.contains("the GPU hold is on, the graphics card is full"),
            "{none}"
        );
        r.fallback = Some(PageFallback {
            alias: "example-cloud/vision".into(),
            local: false,
            outside: outside("example-cloud", "api.example"),
            sees: Some(true),
        });
        let away = fallback_said(&r).unwrap();
        assert!(
            away.contains("pages go to its fallback example-cloud/vision instead"),
            "{away}"
        );
        assert!(
            away.contains("these pages are sent to the service \"example-cloud\" (api.example)"),
            "{away}"
        );
        assert!(
            !away.contains("if "),
            "nothing is left for the reader to work out: {away}"
        );
        r.fallback = Some(PageFallback {
            alias: "example-other".into(),
            local: true,
            outside: None,
            sees: None,
        });
        let here = fallback_said(&r).unwrap();
        assert!(
            here.contains("example-other runs on this computer too"),
            "{here}"
        );
        // A fallback that takes no pictures reads nothing.
        r.fallback = Some(PageFallback {
            alias: "example-text".into(),
            local: false,
            outside: outside("example-cloud", "api.example"),
            sees: Some(false),
        });
        let blind = fallback_said(&r).unwrap();
        assert!(
            blind.contains("takes no pictures and cannot read a page: nothing is sent to it"),
            "{blind}"
        );
        // A model outside has a fallback for its own reasons, and a local
        // one answering for it is "on this computer", not "too".
        let mut c = cloud("example-cloud/vision");
        c.fallback = Some(PageFallback {
            alias: "example-other".into(),
            local: true,
            outside: None,
            sees: None,
        });
        let here = fallback_said(&c).unwrap();
        assert!(
            here.ends_with("example-other runs on this computer."),
            "{here}"
        );
        c.fallback = r.fallback.clone();
        let cloud_said = fallback_said(&c).unwrap();
        assert!(
            cloud_said.contains("out of quota or cannot be reached"),
            "{cloud_said}"
        );
    }

    #[test]
    fn off_says_nothing_is_sent_and_a_choice_names_the_model() {
        assert!(sub_said(&PageReader::default()).starts_with("Off."));
        assert!(sub_said(&local("example-vision")).contains("read by example-vision"));
    }
}
