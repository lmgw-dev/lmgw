//! Settings › Documents' "Layout model" (document-conversion design §3,
//! WP8): the model files lmgw can use for converting, each found or not,
//! with what it adds, its size and licence, its download and its switch.
//! Read with the rest of the page (`doc_tools`' `models`); "Download" is
//! `doc_model_add`, the switch `settings_set`'s `documents_onnx_layout`.
//! Nothing is downloaded or switched on unasked; the expert view adds
//! where the file comes from and lives, its SHA-256, and what the loaded
//! model takes.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::models::{DocModelAddResult, DocModelRow, DocModelState};
use lmgw_api_types::units::fmt_bytes;
use serde_json::json;

use super::{expert_line, use_docs};
use crate::widgets::{use_toasts, Switch};

/// Where the row stands, in a sentence.
pub(super) fn said(r: &DocModelRow) -> String {
    let size = fmt_bytes(r.size_bytes);
    match r.state {
        DocModelState::Found if r.chosen => "In use for PDFs.".to_string(),
        DocModelState::Found => {
            "On this computer, not in use. Switch it on to use it for PDFs.".to_string()
        }
        DocModelState::Missing => {
            let failed = r
                .message
                .as_deref()
                .map(|m| format!(" {m}"))
                .unwrap_or_default();
            format!(
                "Not on this computer. A download of {size} from Hugging Face ({}).{failed}",
                r.licence
            )
        }
        DocModelState::Downloading => {
            format!("Downloading {size}; the Downloads page shows how far it is.")
        }
        DocModelState::WrongFile | DocModelState::NoFolder => r.message.clone().unwrap_or_default(),
    }
}

/// The expert view's line: origin, place, and what the loaded model takes.
pub(super) fn detail(r: &DocModelRow) -> String {
    let place = r
        .path
        .as_deref()
        .map(|p| format!(" · file {p}"))
        .unwrap_or_default();
    let loaded = match r.loaded {
        None => "not in memory".to_string(),
        Some(l) => match l.idle_secs {
            None => format!("in memory, reading a page with {} threads", l.threads),
            Some(s) => format!("in memory with {} threads, last used {s} s ago", l.threads),
        },
    };
    format!(
        "{} · {} at {} · {} · SHA-256 {}{place} · {loaded}; about {} while loaded, up to {} \
         threads, taken together from the parallel tool runs while a page is read (the whole \
         pool where it has no more), dropped {} min after its last page",
        r.licence,
        r.repo,
        r.revision.get(..12).unwrap_or(&r.revision),
        r.file,
        r.sha256,
        fmt_bytes(r.memory_bytes),
        r.threads,
        r.idle_unload_secs / 60
    )
}

#[component]
fn ModelRow(row: DocModelRow) -> impl IntoView {
    let d = use_docs();
    let toasts = use_toasts();
    let busy = RwSignal::new(false);
    let found = row.state == DocModelState::Found;
    let can_download = matches!(row.state, DocModelState::Missing | DocModelState::WrongFile);
    let id = row.id.clone();
    let download = move |_| {
        let id = id.clone();
        busy.set(true);
        spawn_local(async move {
            let body = json!({ "id": id });
            match crate::api::post::<DocModelAddResult, _>("/api/op/doc_model_add", &body).await {
                Ok(r) => toasts.ok(r.message),
                Err(e) => toasts.err(e.to_string()),
            }
            busy.try_set(false);
            d.load(false);
        });
    };
    // The switch saves at once; the page is read again either way, so a
    // refused change shows as what is stored.
    let on = RwSignal::new(row.chosen);
    let stored = row.chosen;
    Effect::new(move |_| {
        let want = on.get();
        if want == stored || busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let body = json!({ "documents_onnx_layout": want });
            match crate::api::post::<serde_json::Value, _>("/api/op/settings_set", &body).await {
                Ok(_) => toasts.ok(if want {
                    "PDFs are laid out with the layout model from now on".to_string()
                } else {
                    "The layout model is off".to_string()
                }),
                Err(e) => toasts.err(e.to_string()),
            }
            busy.try_set(false);
            d.load(false);
        });
    });
    let label = format!("Use it: {}", row.label);
    let off = Signal::derive(move || busy.get() || !(found || stored));
    let sub = said(&row);
    let warn = matches!(
        row.state,
        DocModelState::WrongFile | DocModelState::NoFolder
    );
    let extra = detail(&row);
    view! {
        <div class="prow doc-row" data-model=row.id.clone()>
            <div class="prow-text">
                <span class="prow-title">{row.label.clone()}</span>
                <span class="prow-sub">{row.adds.clone()}</span>
                <span class="prow-sub" class:doc-warn=warn>{sub}</span>
            </div>
            <div class="prow-end">
                {can_download.then(|| view! {
                    <button
                        type="button"
                        class="btn sm"
                        disabled=move || busy.get()
                        on:click=download
                    >
                        "Download"
                    </button>
                })}
                <a href=row.url.clone() target="_blank" rel="noopener noreferrer">"About it"</a>
                <label class="doc-expert-switch">
                    <Switch value=on label=label disabled=off/>
                    <span>"Use it"</span>
                </label>
            </div>
            {expert_line(d.expert, extra)}
        </div>
    }
}

/// The model rows of the page's answer.
#[component]
pub(super) fn ModelRows(rows: Vec<DocModelRow>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <div class="prow notice-row">"No model file is known to this build."</div> }
            .into_any();
    }
    rows.into_iter()
        .map(|row| view! { <ModelRow row=row/> })
        .collect_view()
        .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: DocModelState, chosen: bool) -> DocModelRow {
        DocModelRow {
            id: "layout".into(),
            label: "Layout model (example)".into(),
            adds: "Finds paragraphs.".into(),
            used_for: vec!["PDF".into()],
            state,
            chosen,
            in_use: chosen && state == DocModelState::Found,
            message: None,
            size_bytes: 171_220_471,
            licence: "Apache-2.0".into(),
            repo: "example-owner/example-model".into(),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
            file: "model.onnx".into(),
            sha256: "00ff".into(),
            url: "https://hub.example/example-owner/example-model".into(),
            path: Some("/models.example/example-owner/example-model/model.onnx".into()),
            download_id: None,
            memory_bytes: 430 << 20,
            threads: 4,
            idle_unload_secs: 300,
            loaded: None,
        }
    }

    #[test]
    fn the_row_says_where_the_model_stands() {
        assert_eq!(
            said(&row(DocModelState::Missing, false)),
            "Not on this computer. A download of 163 MiB from Hugging Face (Apache-2.0)."
        );
        assert_eq!(
            said(&row(DocModelState::Downloading, false)),
            "Downloading 163 MiB; the Downloads page shows how far it is."
        );
        assert_eq!(
            said(&row(DocModelState::Found, false)),
            "On this computer, not in use. Switch it on to use it for PDFs."
        );
        assert_eq!(said(&row(DocModelState::Found, true)), "In use for PDFs.");
        let mut wrong = row(DocModelState::WrongFile, false);
        wrong.message = Some("The file at its place is not the model: example.".into());
        assert_eq!(
            said(&wrong),
            "The file at its place is not the model: example."
        );
        let mut failed = row(DocModelState::Missing, false);
        failed.message = Some("Its last download failed: example.".into());
        assert!(said(&failed).ends_with("(Apache-2.0). Its last download failed: example."));
    }

    #[test]
    fn the_expert_line_names_origin_place_and_what_it_takes() {
        let mut r = row(DocModelState::Found, true);
        let d = detail(&r);
        assert!(d.starts_with("Apache-2.0 · example-owner/example-model at 0123456789ab · model.onnx · SHA-256 00ff"), "{d}");
        assert!(
            d.contains("file /models.example/example-owner/example-model/model.onnx"),
            "{d}"
        );
        assert!(
            d.contains("not in memory; about 430 MiB while loaded, up to 4 threads"),
            "{d}"
        );
        assert!(d.ends_with("dropped 5 min after its last page"), "{d}");
        r.loaded = Some(lmgw_api_types::documents::models::DocModelLoaded {
            threads: 4,
            idle_secs: Some(12),
        });
        assert!(detail(&r).contains("in memory with 4 threads, last used 12 s ago"));
    }
}
