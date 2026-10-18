//! The results head's tools for a kept lab (labs-store design §1.7, §2.9):
//! the model filter over the history, and the Export menu (Save ▾ in the
//! media labs, whose entries save files). The Decision lab
//! exports its records: "All as JSONL", "All as CSV", and "<model> as…"
//! while a filter is set. The Image and Audio labs save the files their
//! runs made instead, one zip with a manifest ("Save audio (.zip)", "Save
//! <model> only (.zip)"): a table of generated audio is nothing to keep.
//! The HTTP API still exports their JSONL and CSV. A file is the server's
//! answer with `Content-Disposition: attachment`, started the way a click
//! on `<a download>` is, so the Tauri window asks where to save it.

use leptos::prelude::*;
use lmgw_api_types::lab_store::{LabExportFormat, LabKind};

use super::client::{ExportScope, LabHistory};
use crate::pages::docs::download;
use crate::widgets::{MenuItem, RowMenu, Select};

/// What a media lab's files are called in its menus.
fn media_noun(lab: LabKind) -> Option<&'static str> {
    match lab {
        LabKind::Audio => Some("audio"),
        LabKind::Image => Some("pictures"),
        LabKind::Decision => None,
    }
}

/// `lab`'s menu entries for a filter (or none), as (label, format, scope).
pub(super) fn export_entries(
    lab: LabKind,
    model: Option<&str>,
) -> Vec<(String, LabExportFormat, ExportScope)> {
    if let Some(noun) = media_noun(lab) {
        let mut out = vec![(
            format!("Save {noun} (.zip)"),
            LabExportFormat::Files,
            ExportScope::All,
        )];
        if let Some(m) = model {
            out.push((
                format!("Save {m} only (.zip)"),
                LabExportFormat::Files,
                ExportScope::Model(m.to_string()),
            ));
        }
        return out;
    }
    let mut out = vec![
        (
            "All as JSONL".to_string(),
            LabExportFormat::Jsonl,
            ExportScope::All,
        ),
        (
            "All as CSV".to_string(),
            LabExportFormat::Csv,
            ExportScope::All,
        ),
    ];
    if let Some(m) = model {
        let scope = ExportScope::Model(m.to_string());
        out.push((
            format!("{m} as JSONL"),
            LabExportFormat::Jsonl,
            scope.clone(),
        ));
        out.push((format!("{m} as CSV"), LabExportFormat::Csv, scope));
    }
    out
}

/// The filter and the Export menu. `models` is what the filter offers: the
/// lab's models.
#[component]
pub(in crate::pages) fn ExportMenu(
    history: LabHistory,
    #[prop(into)] models: Signal<Vec<String>>,
) -> impl IntoView {
    // "" is every model.
    let pick = RwSignal::new(history.model.get_untracked().unwrap_or_default());
    // The filter set elsewhere (an ask it would hide clears it): the select
    // follows.
    Effect::new(move |_| {
        let m = history.model.get().unwrap_or_default();
        if pick.get_untracked() != m {
            pick.set(m);
        }
    });
    // A pick: the history under it.
    Effect::new(move |_| {
        let v = pick.get();
        if history.model.get_untracked().unwrap_or_default() != v {
            history.model.set((!v.is_empty()).then(|| v.clone()));
            history.reload();
        }
    });
    let options = Signal::derive(move || {
        std::iter::once((String::new(), "all models".to_string()))
            .chain(models.get().into_iter().map(|m| (m.clone(), m)))
            .collect::<Vec<_>>()
    });
    let (menu_title, menu_label) = if media_noun(history.lab).is_some() {
        ("Save the files the kept runs made", "Save ▾")
    } else {
        ("Export the kept history as a file", "Export ▾")
    };
    let items = Signal::derive(move || {
        let model = history.model.get();
        export_entries(history.lab, model.as_deref())
            .into_iter()
            .map(|(label, format, scope)| {
                let href = history.export_href(format, &scope);
                MenuItem::new(label, move || download(&href, ""))
            })
            .collect::<Vec<_>>()
    });
    view! {
        <span class="lh-filter" title="Only the groups that ask this model">
            <Select value=pick options=options placeholder="all models"/>
        </span>
        <span class="lh-export">
            <RowMenu
                items=items
                title=menu_title
                label=Signal::derive(move || menu_label.to_string())
            />
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_adds_this_models_entries() {
        let all: Vec<String> = export_entries(LabKind::Decision, None)
            .into_iter()
            .map(|e| e.0)
            .collect();
        assert_eq!(all, ["All as JSONL", "All as CSV"]);
        let some = export_entries(LabKind::Decision, Some("decide/laya"));
        assert_eq!(some.len(), 4);
        assert_eq!(some[3].0, "decide/laya as CSV");
        assert_eq!(some[3].2, ExportScope::Model("decide/laya".into()));
    }

    #[test]
    fn a_media_lab_saves_its_files_not_a_table() {
        let all = export_entries(LabKind::Audio, None);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "Save audio (.zip)");
        assert_eq!(
            (all[0].1, &all[0].2),
            (LabExportFormat::Files, &ExportScope::All)
        );
        let some = export_entries(LabKind::Image, Some("image/sdxl"));
        let labels: Vec<&str> = some.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            labels,
            ["Save pictures (.zip)", "Save image/sdxl only (.zip)"]
        );
        assert!(some.iter().all(|e| e.1 == LabExportFormat::Files));
        assert_eq!(some[1].2, ExportScope::Model("image/sdxl".into()));
    }
}
