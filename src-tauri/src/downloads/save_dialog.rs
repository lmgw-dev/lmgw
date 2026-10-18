//! The Save As dialog every download of the window asks with (Linux).
//!
//! The shell owns WebKitGTK's downloads itself rather than through wry's
//! handler: wry calls that one synchronously inside `decide-destination`,
//! so a dialog there would have to block the GTK main thread (the window
//! freezes, KWin marks it unresponsive). Here the signal is answered
//! asynchronously: the handler returns `TRUE` without a destination, which
//! since WebKitGTK 2.40 holds the download until `set_destination` or
//! `cancel` (current WebKitGTK releases have it), and a `GtkFileChooserNative`
//! answers when the owner does. Through the desktop's file-chooser portal
//! when `GTK_USE_PORTAL=1` (`crate::portal`): KDE's own dialog on Plasma.
//!
//! Hooked once per web context: a window rebuilt after a gateway restart
//! gets the same context, and two handlers would ask twice.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use gtk::prelude::*;
use tauri::Manager;
use webkit2gtk::glib::WeakRef;
use webkit2gtk::{Download, DownloadExt, URIRequestExt, WebContext, WebContextExt, WebViewExt};

use super::{fallback_destination, notice, DownloadEnv, Notice, SaveSession};

thread_local! {
    /// The folder chosen last, for this app run (GTK main thread only).
    static SESSION: RefCell<SaveSession> = RefCell::default();
    /// The web contexts already hooked.
    static HOOKED: RefCell<Vec<WeakRef<WebContext>>> = const { RefCell::new(Vec::new()) };
}

/// What became of one download, for its `finished` notice.
#[derive(Default)]
struct Fate {
    /// The owner pressed Cancel: nothing to say.
    canceled: bool,
    /// Why it failed: the shell's reason first, else WebKit's error.
    failed: Option<String>,
    /// The Save As dialog while it is open: a download that fails meanwhile
    /// closes it, so a later pick never reaches a finished download.
    dialog: Option<WeakRef<gtk::FileChooserNative>>,
}

/// Ask with a Save As dialog for every download of `window`'s web context.
pub(crate) fn install(window: &tauri::WebviewWindow) {
    let app = window.app_handle().clone();
    let result = window.with_webview(move |platform| {
        let Some(context) = platform.inner().context() else {
            tracing::warn!("the window has no web context: downloads are not saved");
            return;
        };
        let fresh = HOOKED.with(|hooked| {
            let mut hooked = hooked.borrow_mut();
            hooked.retain(|w| w.upgrade().is_some());
            if hooked
                .iter()
                .any(|w| w.upgrade().as_ref() == Some(&context))
            {
                return false;
            }
            hooked.push(context.downgrade());
            true
        });
        if fresh {
            context.connect_download_started(move |_, download| watch(&app, download));
        }
    });
    if let Err(e) = result {
        tracing::error!("installing the download dialog: {e}");
    }
}

/// Follow one download: ask where it goes, then tell the page how it ended.
fn watch(app: &tauri::AppHandle, download: &Download) {
    let fate = Rc::new(RefCell::new(Fate::default()));
    download.connect_decide_destination({
        let fate = fate.clone();
        move |download, suggested| {
            ask(download, suggested, &fate);
            // Answered: now or when the dialog is.
            true
        }
    });
    download.connect_failed({
        let fate = fate.clone();
        move |_, error| {
            let dialog = {
                let mut f = fate.borrow_mut();
                if f.failed.is_none() {
                    f.failed = Some(error.to_string());
                }
                f.dialog.take()
            };
            if let Some(dialog) = dialog.and_then(|d| d.upgrade()) {
                dialog.hide();
            }
        }
    });
    let app = app.clone();
    download.connect_finished(move |download| {
        let f = fate.borrow();
        let url = download
            .request()
            .and_then(|r| r.uri())
            .map(|u| u.to_string())
            .unwrap_or_default();
        let destination = download.destination();
        let Some(n) = notice(
            f.canceled,
            f.failed.as_deref(),
            destination.as_deref(),
            &url,
        ) else {
            tracing::info!("download canceled in the dialog");
            return;
        };
        match &n {
            Notice::Saved(p) => tracing::info!("download saved: {p}"),
            Notice::Failed(e) => tracing::warn!("download failed: {e}"),
        }
        if let Some(window) = app.get_webview_window("main") {
            if let Err(e) = window.eval(n.js()) {
                tracing::warn!("telling the page about a download: {e}");
            }
        }
    });
}

/// Show the Save As dialog for `download`, suggested as `suggested`, and
/// set its destination or cancel it when the owner answers. With no window
/// to ask in, it goes to the Downloads folder.
fn ask(download: &Download, suggested: &str, fate: &Rc<RefCell<Fate>>) {
    let parent = download
        .web_view()
        .and_then(|v| v.toplevel())
        .and_then(|w| w.downcast::<gtk::Window>().ok());
    let Some(parent) = parent else {
        match fallback_destination(Path::new(suggested)) {
            Ok(p) => download.set_destination(&p.to_string_lossy()),
            Err(e) => {
                tracing::warn!("download refused: {e}");
                fate.borrow_mut().failed = Some(format!("no folder to save into: {e}"));
                download.cancel();
            }
        }
        return;
    };
    let env = DownloadEnv::from_process();
    let start = SESSION.with(|s| s.borrow().start(suggested, &env, Path::is_dir));
    let dialog = gtk::FileChooserNative::new(
        Some("Save As"),
        Some(&parent),
        gtk::FileChooserAction::Save,
        Some("_Save"),
        Some("_Cancel"),
    );
    dialog.set_modal(true);
    dialog.set_do_overwrite_confirmation(true);
    if let Some(dir) = &start.folder {
        dialog.set_current_folder(dir);
    }
    dialog.set_current_name(&start.name);
    // GTK keeps no reference to a native dialog while it is shown: this one
    // is held until it answers.
    let held = Rc::new(RefCell::new(Some(dialog.clone())));
    let download = download.clone();
    let fate = fate.clone();
    fate.borrow_mut().dialog = Some(dialog.downgrade());
    dialog.connect_response(move |dialog, response| {
        let _gone_after_this = held.borrow_mut().take();
        // The download failed while the dialog was open: nothing to answer.
        if fate.borrow().failed.is_some() {
            return;
        }
        fate.borrow_mut().dialog = None;
        match (response, dialog.filename()) {
            (gtk::ResponseType::Accept, Some(path)) => {
                let dest = SESSION.with(|s| s.borrow_mut().chosen(path));
                // The dialog asked before replacing a file; WebKit's default
                // is to fail on one.
                download.set_allow_overwrite(true);
                download.set_destination(&dest.to_string_lossy());
            }
            (gtk::ResponseType::Accept, None) => {
                // A portal can answer with a URI that is not a local file.
                tracing::warn!("download refused: the chosen place is not a local folder");
                fate.borrow_mut().failed =
                    Some("only a local folder can be saved into".to_string());
                download.cancel();
            }
            (gtk::ResponseType::DeleteEvent, _) if crate::portal::in_use() => {
                // GTK 3.24 answers a portal call that failed (no backend on
                // the bus, the call errored) with DELETE_EVENT, and answers
                // the portal's own "cancelled" (Escape, the close button,
                // Cancel: response 1) with CANCEL; any other portal response
                // is DELETE_EVENT too. A closed dialog therefore arrives as
                // Cancel and is silent; DeleteEvent is shown as a failure,
                // so a Save that does nothing is never silent. The cost: a
                // backend that reports a closed window as "other" (2) would
                // show a failure toast instead of nothing.
                tracing::warn!("download refused: the desktop's file dialog did not open");
                fate.borrow_mut().failed =
                    Some("the desktop's file dialog did not open".to_string());
                download.cancel();
            }
            _ => {
                fate.borrow_mut().canceled = true;
                download.cancel();
            }
        }
    });
    dialog.show();
}
