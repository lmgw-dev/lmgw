//! An image call on a route that is not a local image row — a cloud alias,
//! or a local row's GPU-hold fallback — over the OpenAI image routes,
//! synchronously (chat-media-tools design §4.1 step 5).
//!
//! Generations go as JSON (`model`, `prompt`, `size`), edits as multipart
//! with the picture. **Nothing is smuggled**: what the OpenAI body cannot
//! say — seed, steps, cfg, negative prompt, strength — is not sent and is
//! reported as not applied (design §12, decision 16).
//!
//! **The pictures are asked for as base64** (`response_format: b64_json`):
//! dall-e and most OpenAI-compatible providers answer with URLs by default,
//! and lmgw fetches no URLs for tools ([`pictures`]). OpenAI's `gpt-image`
//! models always answer base64 and do not take the parameter
//! ([`response_format`]).

use base64::Engine as _;
use serde_json::{json, Value};

use crate::capabilities::{IMAGE_EDITS_ENDPOINT, IMAGE_GENERATIONS_ENDPOINT};
use crate::config::Route;
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::state::SharedState;

use super::{Action, Done, EditKind, Effective, ImageCall, Picture};

/// The parameters the call set that this route does not take.
fn not_applied(call: &ImageCall<'_>) -> Vec<&'static str> {
    let p = &call.params;
    let mut out = Vec::new();
    if p.seed.is_some() {
        out.push("seed");
    }
    if p.steps.is_some() {
        out.push("steps");
    }
    if p.cfg.is_some() {
        out.push("cfg");
    }
    if p.negative_prompt.as_deref().is_some_and(|n| !n.is_empty()) {
        out.push("negative_prompt");
    }
    if p.strength.is_some() {
        out.push("strength");
    }
    out
}

/// The `response_format` to send for `model` (module doc): `b64_json`, or
/// none for an OpenAI `gpt-image` model (`gpt-image-1`, `gpt-image-1-mini`,
/// `chatgpt-image-latest`), whose API reference says the parameter is not
/// supported there and the answer is always base64.
fn response_format(model: &str) -> Option<&'static str> {
    let name = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    (!name.contains("gpt-image")).then_some("b64_json")
}

pub(super) async fn run(
    state: &SharedState,
    route: &Route,
    call: &ImageCall<'_>,
) -> Result<Done, GatewayError> {
    let size = call.params.size.map(|(w, h)| format!("{w}x{h}"));
    let resp = match &call.action {
        Action::Generate => {
            let mut body = json!({
                "model": route.upstream_model,
                "prompt": call.params.prompt,
                "n": 1,
            });
            if let Some(size) = &size {
                body["size"] = json!(size);
            }
            if let Some(f) = response_format(&route.upstream_model) {
                body["response_format"] = json!(f);
            }
            let path = IMAGE_GENERATIONS_ENDPOINT.trim_start_matches("/v1");
            super::super::image::image_send(state, None, route, |r| {
                Ok(apply_bearer_auth(
                    state
                        .http
                        .post(format!("{}{path}", r.upstream.base()))
                        .json(&body),
                    &r.upstream,
                ))
            })
            .await?
        }
        // A cloud model has no variation route: a variation that lands on
        // one (a local row's fallback) goes as an edit, its strength
        // reported as not applied.
        Action::Edit { image } | Action::Vary { image } => {
            // Judged as `/v1/images/edits` judges it: a catalog that says the
            // model takes no image refuses it here.
            let snap = state.snapshot();
            super::super::image_route_guard(state, &snap, route, call.alias, IMAGE_EDITS_ENDPOINT)
                .await?;
            let path = IMAGE_EDITS_ENDPOINT.trim_start_matches("/v1");
            let ext = image.mime.rsplit('/').next().unwrap_or("png");
            super::super::image::image_send(state, None, route, |r| {
                let part = reqwest::multipart::Part::bytes(image.bytes.clone())
                    .file_name(format!("image.{ext}"))
                    .mime_str(&image.mime)
                    .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
                let mut form = reqwest::multipart::Form::new()
                    .text("model", r.upstream_model.clone())
                    .text("prompt", call.params.prompt.clone())
                    .part("image", part);
                if let Some(size) = &size {
                    form = form.text("size", size.clone());
                }
                if let Some(f) = response_format(&r.upstream_model) {
                    form = form.text("response_format", f);
                }
                Ok(apply_bearer_auth(
                    state
                        .http
                        .post(format!("{}{path}", r.upstream.base()))
                        .multipart(form),
                    &r.upstream,
                ))
            })
            .await?
        }
    };
    let bytes = resp.bytes().await.map_err(GatewayError::from)?;
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let images = pictures(&v)?;
    Ok(Done {
        effective: Effective {
            width: call.params.size.map(|s| s.0),
            height: call.params.size.map(|s| s.1),
            ..Default::default()
        },
        images,
        job_ms: None,
        not_applied: not_applied(call),
        edit: call.action.input().map(|_| EditKind::Cloud),
    })
}

/// The pictures of an OpenAI images answer (`data[].b64_json`). One sent
/// by URL only is refused by name: lmgw does not fetch URLs for tools.
fn pictures(v: &Value) -> Result<Vec<Picture>, GatewayError> {
    let mut out = Vec::new();
    let mut urls = 0;
    for d in v
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match d.get("b64_json").and_then(Value::as_str) {
            Some(b64) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| GatewayError::Upstream {
                        status: 502,
                        provider_type: None,
                        message: format!("the image model's picture is not base64: {e}"),
                    })?;
                let mime = sniff_mime(&bytes);
                out.push(Picture {
                    mime: mime.to_string(),
                    bytes,
                });
            }
            None if d.get("url").is_some() => urls += 1,
            None => {}
        }
    }
    if out.is_empty() {
        let message = if urls > 0 {
            "the image model answered with a URL instead of the picture, and lmgw does not \
             fetch URLs for tools: use a model that returns b64_json"
                .to_string()
        } else {
            "the image model answered with no picture".to_string()
        };
        return Err(GatewayError::Upstream {
            status: 502,
            provider_type: None,
            message,
        });
    }
    Ok(out)
}

/// A picture's type from its first bytes; PNG when they say nothing known.
fn sniff_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/png"
    }
}

#[cfg(test)]
mod tests {
    use super::response_format;

    #[test]
    fn base64_is_asked_for_except_where_the_model_refuses_the_parameter() {
        for m in [
            "dall-e-3",
            "dall-e-2",
            "flux-schnell",
            "black-forest-labs/flux.1-dev",
        ] {
            assert_eq!(response_format(m), Some("b64_json"), "{m}");
        }
        for m in [
            "gpt-image-1",
            "gpt-image-1-mini",
            "openai/gpt-image-1",
            "chatgpt-image-latest",
        ] {
            assert_eq!(response_format(m), None, "{m}");
        }
    }
}
