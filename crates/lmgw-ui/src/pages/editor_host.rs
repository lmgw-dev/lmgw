//! Where an editor form sits: in its modal, or in the Models detail pane
//! (`models/detail/embedded.rs`), which has no modal around it. A form tells
//! its host when its input was saved, when a button changed it (no `input`
//! event), and when it is done, the same way in both: [`use_editor_host`].

use leptos::prelude::*;

use lmgw_ui_kit::widgets::modal::ModalState;

use crate::widgets::use_modal;

/// The pane's side of an embedded form: whether it has unsaved input (the
/// pane's dirty guard reads it, and an untouched form is mounted again when
/// the list reloads), and whether it is still open.
#[derive(Clone, Copy)]
pub(crate) struct PaneEditor {
    pub(crate) touched: RwSignal<bool>,
    pub(crate) open: RwSignal<bool>,
}

impl PaneEditor {
    /// Hand the pane's state to the forms rendered under the caller.
    pub(crate) fn provide(self) {
        provide_context(self);
    }
}

/// The host of the calling form.
#[derive(Clone, Copy)]
pub(crate) enum EditorHost {
    Modal(ModalState),
    Pane(PaneEditor),
}

impl EditorHost {
    /// The input is saved and the form stays open: leaving or closing it
    /// must not ask about changes that are no longer unsaved.
    pub(crate) fn saved(self) {
        match self {
            Self::Modal(m) => m.saved(),
            Self::Pane(p) => p.touched.set(false),
        }
    }

    /// An edit that fired no `input`/`change` (a row added or taken off, a
    /// file picked in a browser): it counts as unsaved input from now on.
    pub(crate) fn touch(self) {
        match self {
            Self::Modal(m) => m.touch(),
            Self::Pane(p) => {
                if !p.touched.get_untracked() {
                    p.touched.set(true);
                }
            }
        }
    }

    /// Done here. A modal asks about unsaved input first, as its ✕ does; the
    /// pane puts the form back to the row as saved, which is what its Cancel
    /// says.
    pub(crate) fn request_close(self) {
        match self {
            Self::Modal(m) => m.request_close(),
            Self::Pane(p) => p.open.set(false),
        }
    }
}

/// The modal the caller renders inside, else the pane it is embedded in.
pub(crate) fn use_editor_host() -> Option<EditorHost> {
    use_modal()
        .map(EditorHost::Modal)
        .or_else(|| use_context::<PaneEditor>().map(EditorHost::Pane))
}
