//! A test host for the Chat (chat-plane design, "WP3a: the transport seam"),
//! in debug builds only: with `localStorage["lmgw-test-host"] = "relay"` the
//! dashboard hands its Chat what a host that relays frames would, so the
//! paths the dashboard itself never takes run in a real browser
//! (`scripts/csp-check.sh`):
//!
//! - a transport with no link of its own ([`Relay`]): pictures and
//!   recordings come as bytes and show by object URLs, downloads are saved
//!   from blobs, a blob body goes as its bytes, every answer is read whole
//!   or as a stream as a relay would hand it, and every request it sends
//!   carries `x-lmgw-test-relay: 1`, so the check's proxy can tell the
//!   Chat's requests that went through it from any that did not;
//! - no stream of the shell's, so the Chat follows its own feed through that
//!   transport;
//! - no machine slot (`ChatHost::machine` is `None`), so the Chat shows no
//!   banner and no card above its empty state, and a `gpu_setup` card is its
//!   sentence alone, as a paired device's sidebar shows them (machine-check
//!   design D178);
//! - the HTML preview in a page of the host's that receives the HTML by
//!   `postMessage` (`PreviewFrame::Posted`, served by the check's proxy).
//!
//! The `/chat` route asks [`preview_page`] (`crate::chat_host`).
//!
//! A release build has none of it.

use leptos::prelude::*;

/// Whether the test host is on.
fn on() -> bool {
    #[cfg(debug_assertions)]
    {
        window()
            .local_storage()
            .ok()
            .flatten()
            .and_then(|s| s.get_item("lmgw-test-host").ok().flatten())
            .is_some_and(|v| v == "relay")
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

/// Provide the relaying transport when the test host is on. Call in `App`,
/// before anything reads the transport (the model catalog captures it).
pub fn provide() {
    #[cfg(debug_assertions)]
    if on() {
        provide_context(crate::api::Http::new(relay::Relay));
    }
}

/// The test host's preview page when it is on: the Chat previews there and
/// follows its own feed (`crate::chat_host`); `None` otherwise.
pub fn preview_page() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        on().then(|| PREVIEW_HOST.to_string())
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// The test host's preview page (`scripts/csp-check/preview-host.html`,
/// served by the check's proxy).
#[cfg(debug_assertions)]
const PREVIEW_HOST: &str = "/__csp-check/preview-host.html";

#[cfg(debug_assertions)]
mod relay {
    use futures::future::LocalBoxFuture;
    use lmgw_ui_kit::http::{
        blob_bytes, Body, Error, Http, Payload, Read, Request, Response, Transport,
    };

    /// The header every relayed request carries (`scripts/csp-check/proxy.py`
    /// flags a Chat request without it while the drive runs the test host).
    pub const TAG: &str = "x-lmgw-test-relay";

    /// The browser's fetch underneath, handed back as a relay hands it: no
    /// link, a blob body sent as its bytes, a blob answer read as whole
    /// bytes, a stream as its chunks. The request's signal goes along, so a
    /// stop aborts it (the cancel a relay sends as a frame).
    pub struct Relay;

    impl Transport for Relay {
        fn send(&self, mut req: Request) -> LocalBoxFuture<'static, Result<Response, String>> {
            Box::pin(async move {
                let wanted = req.read;
                if wanted == Read::Blob {
                    req.read = Read::Whole;
                }
                // A frame carries bytes: a picked file is read, as the
                // sidebar's transport reads it (`arrayBuffer()`), its type
                // the request's when no header names one.
                if let Body::Blob(blob) = &req.body {
                    let kind = blob.type_();
                    let bytes = blob_bytes(blob).await?;
                    if req.header_value("content-type").is_none() && !kind.is_empty() {
                        req = req.header("content-type", &kind);
                    }
                    req.body = Body::Bytes(bytes);
                }
                let req = req.header(TAG, "1");
                let resp = Http::fetch().send(req).await.map_err(|e| match e {
                    Error::Transport(why) => why,
                    other => other.to_string(),
                })?;
                let (status, headers) = (resp.status, resp.headers.clone());
                Ok(match (wanted, resp.body) {
                    (Read::Stream, Payload::Stream(s)) => Response::streamed(status, headers, s),
                    (_, body) => {
                        let bytes = Response {
                            status,
                            headers: Vec::new(),
                            body,
                        }
                        .bytes()
                        .await?;
                        Response::whole(status, headers, bytes)
                    }
                })
            })
        }
    }
}
