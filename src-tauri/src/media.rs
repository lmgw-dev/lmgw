//! The window's microphone permission (chat-voice §13.1), and the same
//! origin rule for the shell's own commands (§13.2). Linux only.
//!
//! wry installs no `permission-request` handler, so WebKitGTK's default
//! denies every request. This one grants exactly one thing: a
//! `getUserMedia` for audio — no video, no display capture — while the
//! window's document is the gateway's own origin (scheme, host and port of
//! what this process serves now, [`crate::gateway::Serving`]: nothing while
//! it serves nothing, the new origin after a restart moved it; not
//! [`crate::navigation_allowed`], which also admits agent-app hosts). The enumerate-devices request
//! (`DeviceInfoPermissionRequest`) is granted on the same origin; WebKitGTK
//! 2.54 was not seen sending one — it shows device labels once a capture has
//! been granted in the document. Every other request goes on to WebKit's
//! default handler, which denies it, as before.
//!
//! **Which document.** `webkit_web_view_get_uri` is the *active* URI: from
//! the moment a main-frame load starts it is the provisional URL, while the
//! old document keeps running until the new one commits — and survives when
//! it never does (a download, a 204, `window.stop()`). An agent app's
//! document could navigate to the gateway and ask in that window. So the
//! running document's URI is tracked from `load-changed` (committed, or
//! finished with no other load started), and a request is granted only when
//! that URI **and** the active one are both the gateway's origin; when they
//! differ the answer is "navigation in progress" (review M1). The shell's
//! commands ask the same question ([`command_allowed`]) from the invoke
//! handler, which Tauri runs on this same GTK main thread as each message
//! arrives, so no commit can fall between the message and the answer.
//!
//! **Frames.** The handler sees only the top-level document. What keeps an
//! agent app's frame or the chat's sandboxed HTML preview off the microphone
//! is WebKit's Permissions Policy: a cross-origin or opaque frame gets no
//! microphone unless its `<iframe>` delegates it with `allow="microphone"`
//! (measured on WebKitGTK 2.54: without it the request never reaches this
//! handler; with it, it arrives under the gateway's URI and would be
//! granted). The dashboard never sets an `allow` attribute, and
//! `tests::the_dashboard_never_delegates_a_frame_permission` keeps it so.
//!
//! **Mock devices** (`LMGW_MOCK_CAPTURE=1`, debug builds only) replace the
//! capture devices with WebKit's mock ones — four "Mock audio device"s and a
//! mock camera — so a check can open "the microphone" in the real shell
//! without one. Requests still go through this handler.

use std::cell::RefCell;
use std::rc::Rc;

use webkit2gtk::glib::prelude::*;
use webkit2gtk::glib::translate::ToGlibPtr;
use webkit2gtk::glib::WeakRef;
use webkit2gtk::{
    DeviceInfoPermissionRequest, LoadEvent, PermissionRequest, PermissionRequestExt, SettingsExt,
    UserMediaPermissionRequest, UserMediaPermissionRequestExt, WebView, WebViewExt,
};

use crate::gateway::Serving;
use crate::WindowOrigin;

/// The environment variable that turns on WebKit's mock capture devices.
pub(crate) const MOCK_CAPTURE_VAR: &str = "LMGW_MOCK_CAPTURE";

/// What a permission request asks for, as far as the decision goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ask {
    /// `getUserMedia` / `getDisplayMedia`, by the kinds of device it wants.
    UserMedia {
        audio: bool,
        video: bool,
        display: bool,
    },
    /// `enumerateDevices`' labels and ids.
    DeviceInfo,
    /// Anything else: geolocation, notifications, pointer lock, clipboard,
    /// media keys, storage access, …
    Other,
}

/// What the handler does with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Grant,
    Deny(&'static str),
    /// Not this handler's: WebKit's default (deny) answers it.
    PassOn,
}

/// Whether the window's document is at the gateway: the running document's
/// URI (`committed`) and the webview's active URI (`active`) both have the
/// exact origin this process serves (`serving`; `None` while it serves
/// nothing). `Err` carries the reason for the log and the rejection.
pub(crate) fn document_at_gateway(
    committed: Option<&str>,
    active: Option<&str>,
    serving: Option<&WindowOrigin>,
) -> Result<(), &'static str> {
    let Some(origin) = serving else {
        return Err("the gateway is not serving");
    };
    let at = |u: Option<&str>| u.is_some_and(|u| is_gateway_origin(u, origin));
    match (at(committed), at(active)) {
        (true, true) => Ok(()),
        (false, false) => Err("not the gateway's origin"),
        // A load is under way, or the first one has not committed: the
        // document that asked is not the one the active URI names.
        _ => Err("navigation in progress"),
    }
}

/// The decision for `ask`, given where the window's document is
/// ([`document_at_gateway`]).
pub(crate) fn decide(ask: Ask, document: Result<(), &'static str>) -> Decision {
    match ask {
        Ask::Other => Decision::PassOn,
        Ask::UserMedia { display: true, .. } => Decision::Deny("display capture"),
        Ask::UserMedia { video: true, .. } => Decision::Deny("video"),
        Ask::UserMedia { audio: false, .. } => Decision::Deny("no audio device asked for"),
        Ask::UserMedia { .. } | Ask::DeviceInfo => match document {
            Ok(()) => Decision::Grant,
            Err(why) => Decision::Deny(why),
        },
    }
}

/// Whether `uri` has exactly the window's scheme, host and port, and no
/// userinfo.
pub(crate) fn is_gateway_origin(uri: &str, origin: &WindowOrigin) -> bool {
    let Ok(url) = uri.parse::<tauri::Url>() else {
        return false;
    };
    origin.same_scheme_and_port(&url)
        && url
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&origin.host))
}

fn ask_of(request: &PermissionRequest) -> Ask {
    if let Some(media) = request.downcast_ref::<UserMediaPermissionRequest>() {
        // The safe binding has no display-device accessor; the C function
        // exists since WebKitGTK 2.34.
        let display = unsafe {
            webkit2gtk::ffi::webkit_user_media_permission_is_for_display_device(
                media.to_glib_none().0,
            )
        } != 0;
        Ask::UserMedia {
            audio: media.is_for_audio_device(),
            video: media.is_for_video_device(),
            display,
        }
    } else if request.is::<DeviceInfoPermissionRequest>() {
        Ask::DeviceInfo
    } else {
        Ask::Other
    }
}

/// Whether the mock devices are on: `LMGW_MOCK_CAPTURE=1` in a debug build.
/// A release build ignores the variable (and says so), so a shipped app never
/// hands a page fake microphones.
pub(crate) fn mock_capture_requested(value: Option<&str>, debug_build: bool) -> bool {
    value == Some("1") && debug_build
}

/// A main-frame `load-changed` stage, as far as the running document goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Load {
    /// A load started or was redirected: the old document still runs.
    Provisional,
    /// The new document replaced the old one.
    Committed,
    /// The load is over (or failed after its commit).
    Finished,
}

/// The running document's URI after `event`, given the webview's active URI
/// then and whether a load is under way. A committed load's URI is the
/// document's. A finished one is too, unless another load has already
/// started — then the active URI is that load's provisional one, and the
/// document is still the one it was.
pub(crate) fn running_after(
    event: Load,
    active: Option<&str>,
    loading: bool,
    running: Option<String>,
) -> Option<String> {
    match event {
        Load::Committed => active.map(str::to_string),
        Load::Finished if !loading => active.map(str::to_string),
        Load::Finished | Load::Provisional => running,
    }
}

/// The window's document, for [`command_allowed`]: kept on the GTK main
/// thread, which is where Tauri dispatches commands and where `load-changed`
/// runs. Replaced when a new window is built.
struct WindowDocument {
    view: WeakRef<WebView>,
    serving: Serving,
    running: Rc<RefCell<Option<String>>>,
}

thread_local! {
    static WINDOW_DOCUMENT: RefCell<Option<WindowDocument>> = const { RefCell::new(None) };
}

/// Whether a command may run for the window's document: the same
/// [`document_at_gateway`] question the microphone gets. Tauri's own check
/// (`capabilities/remote-ui.json`) reads only the active URI, which during a
/// navigation is the provisional one.
///
/// Call it from the invoke handler, which Tauri runs on the GTK main thread
/// as each message arrives (wry's script-message and IPC-scheme handlers both
/// run there): `load-changed` runs on the same thread, so nothing commits
/// between the message and this answer. Called anywhere else there is no
/// window document to ask, and the answer is no.
pub(crate) fn command_allowed() -> Result<(), String> {
    WINDOW_DOCUMENT.with(|doc| {
        let doc = doc.borrow();
        let Some(doc) = doc.as_ref() else {
            return Err("no window document on this thread".to_string());
        };
        let Some(view) = doc.view.upgrade() else {
            return Err("the window is gone".to_string());
        };
        let active = view.uri();
        let running = doc.running.borrow();
        let serving = doc.serving.origin();
        document_at_gateway(running.as_deref(), active.as_deref(), serving.as_ref())
            .map_err(str::to_string)
    })
}

/// `uri`'s origin, for the log: the path can carry a thread id.
fn origin_of(uri: Option<&str>) -> String {
    uri.and_then(|u| u.parse::<tauri::Url>().ok())
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|| "no document".into())
}

/// Install the handler on the window's webview (and the mock devices, when
/// asked for), and start tracking its running document. Call once per
/// window, right after it is built. `serving` is read at every decision.
pub(crate) fn install(window: &tauri::WebviewWindow, serving: Serving) {
    let mock_var = std::env::var(MOCK_CAPTURE_VAR).ok();
    let mock = mock_capture_requested(mock_var.as_deref(), cfg!(debug_assertions));
    if mock_var.is_some() && !mock {
        tracing::warn!(
            "{MOCK_CAPTURE_VAR} is set but ignored: mock capture devices are for debug builds, \
             with the value 1"
        );
    }
    let result = window.with_webview(move |platform| {
        let view = platform.inner();
        if mock {
            if let Some(settings) = WebViewExt::settings(&view) {
                settings.set_enable_mock_capture_devices(true);
                tracing::warn!(
                    "{MOCK_CAPTURE_VAR}=1: the window's capture devices are WebKit's mock \
                     devices (dev/test only)"
                );
            }
        }
        // The first load may already be over by now; one still under way
        // reports its commit (or its end) below.
        let running = Rc::new(RefCell::new(if view.is_loading() {
            None
        } else {
            view.uri().map(|u| u.to_string())
        }));
        {
            let running = running.clone();
            view.connect_load_changed(move |view, event| {
                let event = match event {
                    LoadEvent::Committed => Load::Committed,
                    LoadEvent::Finished => Load::Finished,
                    _ => Load::Provisional,
                };
                let uri = view.uri();
                let before = running.take();
                running.replace(running_after(
                    event,
                    uri.as_deref(),
                    view.is_loading(),
                    before,
                ));
            });
        }
        WINDOW_DOCUMENT.with(|doc| {
            doc.replace(Some(WindowDocument {
                view: view.downgrade(),
                serving: serving.clone(),
                running: running.clone(),
            }));
        });
        view.connect_permission_request(move |view, request| {
            let ask = ask_of(request);
            let active = view.uri();
            let running = running.borrow().clone();
            let decision = decide(
                ask,
                document_at_gateway(
                    running.as_deref(),
                    active.as_deref(),
                    serving.origin().as_ref(),
                ),
            );
            // The document that asked is the running one; the active URI is
            // named too when a navigation has moved it elsewhere.
            let from = origin_of(running.as_deref());
            let to = origin_of(active.as_deref());
            let from = if to == from {
                from
            } else {
                format!("{from} (navigating to {to})")
            };
            match decision {
                Decision::Grant => {
                    tracing::info!("media permission: {ask:?} granted for {from}");
                    request.allow();
                    true
                }
                Decision::Deny(why) => {
                    tracing::info!("media permission: {ask:?} denied for {from}: {why}");
                    request.deny();
                    true
                }
                Decision::PassOn => false,
            }
        });
    });
    if let Err(e) = result {
        tracing::error!("installing the media permission handler: {e}");
    }
}

#[cfg(test)]
mod tests;
