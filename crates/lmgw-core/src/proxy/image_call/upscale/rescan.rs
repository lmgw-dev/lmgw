//! A held image server whose last capabilities said `upscale: false`, asked
//! again before the upscale is refused (chat-media-tools design §14 item
//! 34): sd-server rescans `--hires-upscalers-dir` on every capabilities
//! read, so a file that arrived after the container started is found, and
//! the registry keeps the new answer.

use crate::config::Route;
use crate::egress::apply_bearer_auth;
use crate::error::GatewayError;
use crate::runtime::image::ImageCapabilities;
use crate::state::SharedState;

use super::super::native_root;

/// The refusal for a container that says it cannot upscale (module doc):
/// one whose last answer said so is asked again first, and the registry
/// keeps what it answers now.
pub(super) async fn no_upscaler(
    state: &SharedState,
    alias: &str,
    model_id: &str,
    hold: &crate::vram::LocalHold,
    route: &Route,
) -> Option<GatewayError> {
    let caps = state.runtime().image_capabilities(model_id)?;
    if caps.upscale {
        return None;
    }
    if let Some(now) = capabilities_now(state, hold, route).await {
        let upscale = now.upscale;
        state.runtime().refresh_image_capabilities(model_id, now);
        if upscale {
            return None;
        }
    }
    let models_dir = state.snapshot().settings.image.models_dir.clone();
    let on_disk = crate::image_upscalers::names(&models_dir);
    let message = if on_disk.is_empty() {
        format!(
            "{alias}'s image server has no upscaler: {}/ of the image models dir holds no \
             ESRGAN file — image_upscaler_add downloads one",
            crate::image_upscalers::DIR
        )
    } else {
        format!(
            "{alias}'s image server reports no upscaler it can run although {}/ holds {}: \
             sd-server upscales only with an RGB ESRGAN model, and a row whose files set \
             its own hires_upscalers_dir reads that directory instead",
            crate::image_upscalers::DIR,
            on_disk.join(", ")
        )
    };
    Some(GatewayError::Refused {
        status: 422,
        code: "no_upscaler",
        message,
    })
}

/// `GET /sdcpp/v1/capabilities` on the held container, which rescans its
/// upscalers; `None` when it does not answer with a readable document.
async fn capabilities_now(
    state: &SharedState,
    hold: &crate::vram::LocalHold,
    route: &Route,
) -> Option<ImageCapabilities> {
    let resp = crate::vram::send_local(Some(hold), route, route.upstream.request_timeout(), |r| {
        Ok(apply_bearer_auth(
            state
                .http
                .get(format!("{}/sdcpp/v1/capabilities", native_root(r))),
            &r.upstream,
        ))
    })
    .await
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    ImageCapabilities::parse(&resp.text().await.ok()?).ok()
}
