//! The panel's WebSocket to `/v1/realtime?chat_thread=<id>` (chat-voice
//! §8.1), and what a close says.
//!
//! The browser sends the dashboard's cookie with the handshake (the same
//! origin), which is the binding's credential. **A refused handshake is
//! invisible to the page:** the WebSocket API hides the HTTP status of a
//! failed upgrade (403, 404, 409 `chat_thread_admin`), and closes with 1006
//! before it ever opened. The panel then reads the thread again to name the
//! likely reason (`realtime.rs`).
//!
//! **Nothing reconnects by itself** (§8.6): every close is said, with its
//! reason, and the panel offers Re-enter.
//!
//! **The page's own close is answered late, on purpose:** the gateway reads
//! it, ends the session, drains its journal (§8.6) and only then closes its
//! side (IT `the_server_s_close_comes_once_the_journal_drained`). So the
//! browser's close event after [`Socket::close_then`] says the thread holds
//! everything the session wrote (WP9 review m5).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use lmgw_client::realtime::{ErrorFacts, CLOSE_TAKEN_OVER as TAKEN_OVER};

/// What the socket says.
pub(crate) enum SockEvent {
    Open,
    Message(String),
    Close { code: u16, reason: String },
}

type Handlers = (
    Closure<dyn FnMut()>,
    Closure<dyn FnMut(web_sys::MessageEvent)>,
    Closure<dyn FnMut(web_sys::CloseEvent)>,
);

/// An open (or opening) socket. Dropping it closes it.
pub(crate) struct Socket {
    ws: web_sys::WebSocket,
    handlers: RefCell<Option<Handlers>>,
    /// Closed from this side already.
    closed: Cell<bool>,
}

/// Open `url`; `on` hears every event, `Close` last and once.
pub(crate) fn open(url: &str, on: impl FnMut(SockEvent) + 'static) -> Result<Rc<Socket>, String> {
    let ws = web_sys::WebSocket::new(url)
        .map_err(|e| format!("the voice session could not be started: {}", js(&e)))?;
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
    let on = Rc::new(RefCell::new(on));
    let o = on.clone();
    let on_open = Closure::<dyn FnMut()>::new(move || {
        if let Ok(mut f) = o.try_borrow_mut() {
            f(SockEvent::Open);
        }
    });
    let o = on.clone();
    let on_message = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::MessageEvent| {
        if let Some(text) = ev.data().as_string() {
            if let Ok(mut f) = o.try_borrow_mut() {
                f(SockEvent::Message(text));
            }
        }
    });
    let o = on;
    let on_close = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::CloseEvent| {
        if let Ok(mut f) = o.try_borrow_mut() {
            f(SockEvent::Close {
                code: ev.code(),
                reason: ev.reason(),
            });
        }
    });
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    Ok(Rc::new(Socket {
        ws,
        handlers: RefCell::new(Some((on_open, on_message, on_close))),
        closed: Cell::new(false),
    }))
}

impl Socket {
    pub(crate) fn is_open(&self) -> bool {
        self.ws.ready_state() == web_sys::WebSocket::OPEN
    }

    /// Closed, by either side: nothing more can be said on it.
    pub(crate) fn closed(&self) -> bool {
        self.closed.get()
            || matches!(
                self.ws.ready_state(),
                web_sys::WebSocket::CLOSING | web_sys::WebSocket::CLOSED
            )
    }

    /// Send a text frame; `false` when the socket is not open.
    pub(crate) fn send(&self, text: &str) -> bool {
        self.is_open() && self.ws.send_with_str(text).is_ok()
    }

    /// Close it from this side (leaving): no event is said for it.
    pub(crate) fn close(&self) {
        self.close_then(None);
    }

    /// [`Self::close`], and `then` is called once the browser says the
    /// socket closed: the gateway's answer after its drain (module doc), or
    /// the connection going. At once when it is closed already.
    pub(crate) fn close_then(&self, then: Option<Box<dyn FnOnce()>>) {
        if self.closed.replace(true) {
            return;
        }
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        match then {
            Some(f) if self.ws.ready_state() == web_sys::WebSocket::CLOSED => f(),
            Some(f) => {
                let once = Closure::once_into_js(move |_: web_sys::CloseEvent| f());
                self.ws.set_onclose(Some(once.unchecked_ref()));
            }
            None => self.ws.set_onclose(None),
        }
        let _ = self.ws.close_with_code_and_reason(1000, "voice mode left");
        self.handlers.borrow_mut().take();
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // A close of this side's keeps the close event it asked for.
        self.close();
    }
}

fn js(e: &wasm_bindgen::JsValue) -> String {
    super::super::audio::shell::js_text(e)
}

/// What a close nobody here asked for says, in the panel's words. `opened`:
/// the socket had opened (a handshake refused never does); `error`: the
/// last `error` event before it.
pub(crate) fn close_words(
    code: u16,
    reason: &str,
    opened: bool,
    error: Option<&ErrorFacts>,
) -> String {
    let reason = reason.trim();
    if code == TAKEN_OVER || error.and_then(|e| e.code.as_deref()) == Some("chat_thread_taken_over")
    {
        // The close names who took it over — "voice mode moved to device
        // 'phone'", "… to the dashboard" (client-apps design §1.7).
        return if code == TAKEN_OVER && reason.starts_with("voice mode moved") {
            reason.to_string()
        } else {
            "voice mode moved to another window".into()
        };
    }
    if !opened {
        return "the voice session could not be opened: the gateway refused it or could not be \
                reached"
            .into();
    }
    let with = |what: &str| {
        if reason.is_empty() {
            what.to_string()
        } else {
            format!("{what}: {reason}")
        }
    };
    match code {
        1000 => with("the gateway ended the voice session"),
        // The reason says it whole ("lmgw is stopping or restarting").
        1001 if !reason.is_empty() => reason.to_string(),
        1001 => "the gateway is stopping or restarting".into(),
        1006 => "the connection to lmgw was lost (the network, or the gateway stopped)".into(),
        1009 => with("a message was larger than the gateway takes"),
        1011 => with("the gateway ended the session after an error"),
        // A revocation's reason starts with the kind's token for clients
        // that act on it; a person reads the sentence after it (review
        // G-11): "owner key 'dashboard' was rotated".
        lmgw_api_types::realtime::CLOSE_REVOKED => {
            let (_, sentence) = lmgw_api_types::realtime::RevokeKind::split_close_reason(reason);
            if sentence.is_empty() {
                "this key was revoked".into()
            } else {
                format!("the voice session ended: {sentence}")
            }
        }
        // The thread is out of this key's reach; the reason says it neutrally.
        lmgw_api_types::realtime::CLOSE_OUT_OF_REACH if !reason.is_empty() => {
            format!("the voice session ended: {reason}")
        }
        _ => with(&format!("the voice session ended (code {code})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_close_says_why() {
        assert_eq!(
            close_words(4000, "voice mode moved to another window", true, None),
            "voice mode moved to another window"
        );
        assert_eq!(
            close_words(4000, "voice mode moved to device 'phone'", true, None),
            "voice mode moved to device 'phone'",
            "the close names who took it over (client-apps §1.7)"
        );
        let taken = ErrorFacts {
            code: Some("chat_thread_taken_over".into()),
            ..Default::default()
        };
        assert_eq!(
            close_words(1006, "", true, Some(&taken)),
            "voice mode moved to another window"
        );
        assert!(
            close_words(1006, "", false, None).starts_with("the voice session could not be opened")
        );
        assert!(close_words(1006, "", true, None).starts_with("the connection to lmgw was lost"));
        assert_eq!(
            close_words(1011, "the journal failed", true, None),
            "the gateway ended the session after an error: the journal failed"
        );
        assert_eq!(
            close_words(4100, "", true, None),
            "the voice session ended (code 4100)"
        );
        // A revocation's token is a client's, not the reader's.
        assert_eq!(
            close_words(
                4003,
                "revoked: owner key 'dashboard' was rotated",
                true,
                None
            ),
            "the voice session ended: owner key 'dashboard' was rotated"
        );
        assert_eq!(
            close_words(
                4004,
                "chat thread 7 is out of reach for this key",
                true,
                None
            ),
            "the voice session ended: chat thread 7 is out of reach for this key"
        );
    }
}
