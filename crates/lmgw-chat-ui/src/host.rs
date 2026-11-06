//! What a host hands the Chat (chat-plane design, "WP3b: the lmgw-chat-ui
//! crate"). The Chat knows no router, no dashboard session and no page URL:
//! its host says where it was opened and keeps the open thread in its own
//! address ([`ChatLocation`]), says when its session is locked, hands it its
//! live updates and its preview frame, says whether it has voice and
//! where the Chat's assets are served, and hands it the machine check's
//! surfaces where it has them ([`ChatMachine`]). The transport is the kit's
//! (`lmgw_ui_kit::http::Http` in context, provided before the model catalog).
//!
//! A host calls [`provide_chat_host`] once, before it mounts [`crate::Chat`].

use std::cell::RefCell;
use std::sync::Arc;

use leptos::prelude::*;

pub use crate::chat_live::ChatLive;
pub use crate::chat_machine::ChatMachine;
pub use crate::chat_preview::PreviewFrame;

/// What the host's address asked the Chat for when it mounted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatAddress {
    /// The thread to open (the dashboard's `?t=`); `None` opens the first
    /// one listed.
    pub thread: Option<i64>,
    /// A message to scroll to and flash once its thread is in (`?m=`).
    pub message: Option<i64>,
    /// A folder whose settings form opens once the list is in
    /// (`?folder=<id>&settings=1`, client-apps design §3.7).
    pub folder_settings: Option<i64>,
}

/// Where the Chat is, in the host's terms.
///
/// The dashboard reads its router's query and keeps `/chat?t=<id>` in the
/// address bar; a host without an address (the sidebar) keeps the open
/// thread in memory and has no dashboard pages to link to.
pub trait ChatLocation: Send + Sync {
    /// What the address asks for, read once when the Chat mounts.
    fn opened_at(&self) -> ChatAddress;
    /// The open thread changed (`None`: none is open). Called on every
    /// switch, while a reply streams too: it must not remount the Chat.
    fn show_thread(&self, id: Option<i64>);
    /// The href of a page of lmgw's dashboard (`/models`, `/audio-lab`,
    /// `/chat/profiles?thread=3`), or `None` where the host cannot open one:
    /// the Chat then leaves the link out.
    fn dashboard_href(&self, path: &str) -> Option<String>;
}

/// Voice, where the host has it: the realtime session's WebSocket.
#[derive(Clone)]
pub struct ChatVoice {
    /// The WebSocket URL of `/v1/realtime?chat_thread=<id>` for thread `id`
    /// (`lmgw_client::requests::realtime_page_url` for a page served by
    /// lmgw).
    pub realtime_url: Arc<dyn Fn(i64) -> String + Send + Sync>,
}

/// Where the user writes from, asked at every send and every edit of a user
/// message: one sentence the host words ("Typed in Kai's sidebar in
/// Firefox, beside the tab “Example” (https://example.org/)."), which goes
/// as the request's `origin`, is stored on the message and is put before
/// its text in the model's input (chat-plane design §9.10). `None` (or a
/// blank sentence) says nothing.
pub type ChatOrigin = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Everything a host hands the Chat besides the transport.
#[derive(Clone)]
pub struct ChatHost {
    pub location: Arc<dyn ChatLocation>,
    /// True while the host's session is locked: the Chat's own feed closes,
    /// and opens again when it unlocks. A `401 session_required` the Chat
    /// reads runs the kit's hook (`lmgw_ui_kit::http::Config::on_unauthorized`),
    /// which is where the host locks it.
    pub locked: Signal<bool>,
    /// The live updates' source; [`ChatLive::Feed`] by default.
    pub live: ChatLive,
    /// The HTML preview's frame; the sandboxed `srcdoc` by default.
    pub preview: PreviewFrame,
    /// Voice: `None` shows no voice controls at all (the microphone, voice
    /// mode, the voice menu, a reply's speaker, the dictation key).
    pub voice: Option<ChatVoice>,
    /// Where the Chat's assets are served, ending in `/` (`voice/…`, the
    /// worklets and the visualisations); `"/"` by default.
    pub assets: String,
    /// The machine check's surfaces in the Chat (machine-check design D177):
    /// the banner over the conversation, the card above the empty state and
    /// the fix under a `gpu_setup` card. `None` (the default) shows no banner
    /// and no card, and a `gpu_setup` card is its sentence alone: a host
    /// that cannot read the admin plane's check (a paired device's sidebar)
    /// hands none.
    pub machine: Option<ChatMachine>,
    /// Where the user writes from ([`ChatOrigin`]), asked for each send and
    /// each edit; `None` (the default) sends none, and the messages show
    /// none.
    pub origin: Option<ChatOrigin>,
}

impl ChatHost {
    /// A host with the defaults: the Chat follows its own feed, previews in
    /// a sandboxed `srcdoc` frame, has no voice, no machine check and no
    /// origin, and finds its assets at the root.
    pub fn new(location: Arc<dyn ChatLocation>, locked: Signal<bool>) -> Self {
        Self {
            location,
            locked,
            live: ChatLive::Feed,
            preview: PreviewFrame::Srcdoc,
            voice: None,
            assets: "/".into(),
            machine: None,
            origin: None,
        }
    }
}

thread_local! {
    /// The asset base of the page's one host, for the audio code that loads
    /// a worklet long after any reactive owner could be asked.
    static ASSETS: RefCell<String> = RefCell::new("/".into());
}

/// Hand the Chat its host. Call once, before mounting [`crate::Chat`].
pub fn provide_chat_host(host: ChatHost) {
    let base = if host.assets.ends_with('/') {
        host.assets.clone()
    } else {
        format!("{}/", host.assets)
    };
    ASSETS.with(|a| *a.borrow_mut() = base);
    provide_context(host);
}

/// The host, as the page reads it while it is set up.
pub(crate) fn use_chat_host() -> ChatHost {
    use_context::<ChatHost>()
        .expect("lmgw-chat-ui: the host calls provide_chat_host before it mounts the Chat")
}

/// The host's location as a `Copy` handle the page's structs carry, owned by
/// the Chat: once the page is gone it says nothing, so a read that lands
/// late cannot rewrite the next page's address.
#[derive(Clone, Copy)]
pub(crate) struct Place(StoredValue<Arc<dyn ChatLocation>>);

impl Place {
    pub(crate) fn new(location: Arc<dyn ChatLocation>) -> Self {
        Self(StoredValue::new(location))
    }

    /// [`ChatLocation::show_thread`], while the page lives.
    pub(crate) fn show_thread(self, id: Option<i64>) {
        if let Some(l) = self.0.try_get_value() {
            l.show_thread(id);
        }
    }

    /// [`ChatLocation::dashboard_href`].
    pub(crate) fn href(self, path: &str) -> Option<String> {
        self.0.try_get_value().and_then(|l| l.dashboard_href(path))
    }
}

/// The host's [`ChatOrigin`] as a `Copy` handle owned by the Chat.
#[derive(Clone, Copy)]
pub(crate) struct Origin(StoredValue<Option<ChatOrigin>>);

impl Origin {
    pub(crate) fn new(origin: Option<ChatOrigin>) -> Self {
        Self(StoredValue::new(origin))
    }

    /// Where the user writes from now, as the request's `origin`: the
    /// host's sentence trimmed, `None` without one or when it is blank.
    pub(crate) fn now(self) -> Option<String> {
        let ask = self.0.try_get_value().flatten()?;
        origin_text(ask())
    }
}

/// A host's sentence as it is sent: trimmed, and none when blank.
pub(crate) fn origin_text(said: Option<String>) -> Option<String> {
    said.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The host's location as a [`Place`] owned by the calling component.
pub(crate) fn use_place() -> Place {
    Place::new(use_chat_host().location)
}

/// The URL of the Chat's asset `path` (`voice/player-worklet.js`) under the
/// host's base.
pub(crate) fn asset_url(path: &str) -> String {
    ASSETS.with(|a| format!("{}{path}", a.borrow()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_origin_is_none() {
        assert_eq!(origin_text(None), None);
        assert_eq!(origin_text(Some(" \n".into())), None);
        assert_eq!(
            origin_text(Some(" Typed in the sidebar. ".into())).as_deref(),
            Some("Typed in the sidebar.")
        );
    }

    #[test]
    fn an_asset_is_under_the_hosts_base() {
        assert_eq!(asset_url("viz/x.js"), "/viz/x.js");
        ASSETS.with(|a| *a.borrow_mut() = "/chat-assets/".into());
        assert_eq!(asset_url("viz/x.js"), "/chat-assets/viz/x.js");
        ASSETS.with(|a| *a.borrow_mut() = "/".into());
    }
}
