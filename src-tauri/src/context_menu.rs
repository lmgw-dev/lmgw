//! The window's right-click menu: WebKit's own, less what makes it a
//! browser's.
//!
//! On a button or an empty pane WebKit offers Back, Forward and Reload: a web
//! page's menu, where a desktop app has none. Those go, and with nothing left
//! no menu opens at all. What does a desktop job stays: cut, copy, paste and
//! spelling in a text field, Copy on selected text, copy and save on an
//! image, Copy Link, a media element's controls. A debug build keeps Inspect
//! Element beside those, its way into the web inspector, but not as a menu of
//! its own: a pane gets none there either. A release build has no inspector.

use webkit2gtk::{ContextMenuAction, ContextMenuExt, ContextMenuItemExt, WebViewExt};

/// Installs the trim on the window's webview, right after it is built.
pub(crate) fn install(window: &tauri::WebviewWindow) {
    let result = window.with_webview(|platform| {
        platform
            .inner()
            .connect_context_menu(|_view, menu, _event, _hit| {
                let items = menu.items();
                let entries: Vec<Entry> = items
                    .iter()
                    .map(|item| {
                        if item.is_separator() {
                            Entry::Separator
                        } else {
                            Entry::Action(item.stock_action())
                        }
                    })
                    .collect();
                for (item, keep) in items.iter().zip(kept(&entries)) {
                    if !keep {
                        menu.remove(item);
                    }
                }
                // true: handled, so an emptied menu does not open.
                menu.n_items() == 0
            });
    });
    if let Err(e) = result {
        tracing::warn!("right-click menu left as WebKit's: {e}");
    }
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Separator,
    Action(ContextMenuAction),
}

/// A browser's navigation and window actions. The new-window ones would open
/// the system browser on the gateway's own pages (main.rs, on_new_window).
fn browser_only(action: ContextMenuAction) -> bool {
    use ContextMenuAction as A;
    match action {
        A::GoBack
        | A::GoForward
        | A::Stop
        | A::Reload
        | A::OpenLink
        | A::OpenLinkInNewWindow
        | A::DownloadLinkToDisk
        | A::OpenImageInNewWindow
        | A::OpenFrameInNewWindow
        | A::OpenVideoInNewWindow
        | A::OpenAudioInNewWindow => true,
        A::InspectElement => !cfg!(debug_assertions),
        _ => false,
    }
}

/// Which entries stay: no browser action, and a separator only between two
/// items that stay. Inspect Element alone is no menu.
fn kept(entries: &[Entry]) -> Vec<bool> {
    let mut keep: Vec<bool> = entries
        .iter()
        .map(|e| match e {
            Entry::Separator => true,
            Entry::Action(a) => !browser_only(*a),
        })
        .collect();
    // An item that stays, seen since the last separator kept.
    let mut item_before = false;
    for (i, e) in entries.iter().enumerate() {
        match e {
            Entry::Separator => {
                keep[i] = keep[i] && item_before;
                if keep[i] {
                    item_before = false;
                }
            }
            Entry::Action(_) => item_before |= keep[i],
        }
    }
    // A separator kept with no item after it.
    for i in (0..entries.len()).rev() {
        match entries[i] {
            Entry::Separator => keep[i] = false,
            Entry::Action(_) if keep[i] => break,
            Entry::Action(_) => {}
        }
    }
    let only_inspect = entries.iter().zip(&keep).all(|(e, k)| match e {
        Entry::Action(a) => !k || *a == ContextMenuAction::InspectElement,
        Entry::Separator => !k,
    });
    if only_inspect {
        keep.iter_mut().for_each(|k| *k = false);
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;
    use ContextMenuAction as A;
    use Entry::{Action, Separator};

    #[test]
    fn a_pane_gets_no_menu() {
        let page = [Action(A::GoBack), Action(A::GoForward), Action(A::Reload)];
        assert_eq!(kept(&page), [false, false, false]);
        // A debug build's pane: Inspect Element would be all that is left.
        let debug = [
            Action(A::GoBack),
            Action(A::Reload),
            Separator,
            Action(A::InspectElement),
        ];
        assert_eq!(kept(&debug), [false, false, false, false]);
    }

    #[test]
    fn inspect_element_stays_beside_a_real_action_in_debug_builds() {
        let text = [Action(A::Copy), Separator, Action(A::InspectElement)];
        let want = cfg!(debug_assertions);
        assert_eq!(kept(&text), [true, want, want]);
    }

    #[test]
    fn a_text_field_keeps_its_editing_actions() {
        let field = [
            Action(A::Cut),
            Action(A::Copy),
            Action(A::Paste),
            Action(A::Delete),
            Separator,
            Action(A::SelectAll),
        ];
        assert!(kept(&field).iter().all(|k| *k));
    }

    #[test]
    fn a_link_keeps_copy_link_and_no_dangling_separator() {
        let link = [
            Action(A::OpenLink),
            Action(A::OpenLinkInNewWindow),
            Action(A::DownloadLinkToDisk),
            Action(A::CopyLinkToClipboard),
            Separator,
            Action(A::GoBack),
            Action(A::Reload),
        ];
        assert_eq!(
            kept(&link),
            [false, false, false, true, false, false, false]
        );
    }

    #[test]
    fn separators_stay_only_between_kept_items() {
        let menu = [
            Separator,
            Action(A::Reload),
            Separator,
            Action(A::Copy),
            Separator,
            Separator,
            Action(A::CopyImageToClipboard),
            Separator,
        ];
        assert_eq!(
            kept(&menu),
            [false, false, false, true, true, false, true, false]
        );
    }

    #[test]
    fn inspect_element_stays_in_debug_builds_only() {
        assert_eq!(browser_only(A::InspectElement), !cfg!(debug_assertions));
    }
}
