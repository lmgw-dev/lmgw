//! The browser's `fetch` as a [`Transport`]: the dashboard's, under the base
//! [`super::configure`] set. A path is put after the base; the browser's own
//! credentials go along (the dashboard's session cookie), and a link is the
//! same URL, so an `<img>`, an `<audio>` or a download loads it by itself.

use futures::future::LocalBoxFuture;
use futures::stream::StreamExt;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::transport::{Body, Payload, Read, Request, Response, Transport};

/// `fetch`, unchanged from what the kit's free functions send.
pub struct Fetch;

impl Transport for Fetch {
    fn send(&self, req: Request) -> LocalBoxFuture<'static, Result<Response, String>> {
        Box::pin(send(req, None))
    }

    fn link(&self, path: &str) -> Option<String> {
        Some(super::url(path.to_string()))
    }
}

fn js_err(e: JsValue) -> String {
    e.as_string()
        .or_else(|| {
            e.dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
        })
        .unwrap_or_else(|| format!("{e:?}"))
}

/// `req` with a body the browser takes as it is (a `FormData`, a string),
/// for the free [`super::post_raw`]: the dashboard's own pages' uploads, never
/// a page a host may relay (its body is plain data, [`Body`]).
pub(super) async fn send_raw(req: Request, body: JsValue) -> Result<Response, String> {
    send(req, Some(body)).await
}

/// Send `req`; `raw`, when given, is the body instead of `req.body`.
async fn send(req: Request, raw: Option<JsValue>) -> Result<Response, String> {
    let init = web_sys::RequestInit::new();
    init.set_method(req.method.as_str());
    let headers = web_sys::Headers::new().map_err(js_err)?;
    for (k, v) in &req.headers {
        headers.append(k, v).map_err(js_err)?;
    }
    if let Some(raw) = &raw {
        init.set_body(raw);
    }
    match &req.body {
        _ if raw.is_some() => {}
        Body::Empty => {}
        Body::Json(text) => {
            if req.header_value("content-type").is_none() {
                headers
                    .set("content-type", "application/json")
                    .map_err(js_err)?;
            }
            init.set_body(&JsValue::from_str(text));
        }
        Body::Bytes(bytes) => init.set_body(&js_sys::Uint8Array::from(bytes.as_slice())),
        Body::Blob(blob) => init.set_body(blob),
    }
    init.set_headers(&headers);
    if let Some(signal) = &req.signal {
        init.set_signal(Some(signal));
    }
    let window = web_sys::window().ok_or("no window")?;
    let url = super::url(req.path.clone());
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_str_and_init(&url, &init))
        .await
        .map_err(js_err)?
        .unchecked_into();
    let status = resp.status();
    let headers = header_list(&resp.headers());
    let body = match req.read {
        Read::Whole => Payload::Whole(whole(&resp).await?),
        Read::Blob => Payload::Blob(
            JsFuture::from(resp.blob().map_err(js_err)?)
                .await
                .map_err(js_err)?
                .unchecked_into(),
        ),
        Read::Stream => match resp.body() {
            Some(raw) => Payload::Stream(
                wasm_streams::ReadableStream::from_raw(raw)
                    .into_stream()
                    .map(|chunk| {
                        chunk
                            .map(|c| js_sys::Uint8Array::new(&c).to_vec())
                            .map_err(js_err)
                    })
                    .boxed_local(),
            ),
            None => Payload::Whole(Vec::new()),
        },
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

async fn whole(resp: &web_sys::Response) -> Result<Vec<u8>, String> {
    let buf = JsFuture::from(resp.array_buffer().map_err(js_err)?)
        .await
        .map_err(js_err)?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

fn header_list(h: &web_sys::Headers) -> Vec<(String, String)> {
    let Ok(Some(iter)) = js_sys::try_iter(h) else {
        return Vec::new();
    };
    iter.filter_map(Result::ok)
        .filter_map(|pair| {
            let pair: js_sys::Array = pair.dyn_into().ok()?;
            Some((
                pair.get(0).as_string()?.to_ascii_lowercase(),
                pair.get(1).as_string()?,
            ))
        })
        .collect()
}

/// A blob's bytes (copied into the wasm module's memory): how a host that
/// relays frames sends a [`Body::Blob`].
pub async fn blob_bytes(blob: &web_sys::Blob) -> Result<Vec<u8>, String> {
    let buf = JsFuture::from(blob.array_buffer()).await.map_err(js_err)?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

/// A blob of `bytes` with type `kind` (none when empty).
pub(super) fn blob_of(bytes: &[u8], kind: &str) -> Result<web_sys::Blob, String> {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    if !kind.is_empty() {
        opts.set_type(kind);
    }
    web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js_err)
}
