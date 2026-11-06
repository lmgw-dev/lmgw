//! The capabilities each image row's containers last answered with, kept
//! after the container stops (chat-media-tools review, finding 5).
//!
//! A running container's capabilities document ([`RuntimeView`]'s
//! `image_capabilities`) goes with its entry. The media tools' descriptions
//! read a row's defaults and size range from it, and a tool block that
//! changed whenever the row's container came up or went idle would change
//! the prompt prefix of every chat turn that has the tools — a local chat
//! model's prompt cache would be thrown away turn after turn. So the last
//! document of each row is remembered here, in memory, for this process:
//! a row reads the same from its first start on, and a start that answers
//! differently (a new build, edited files) replaces it.

use crate::runtime::image::ImageCapabilities;
use crate::runtime::Class;

use super::Registry;

impl Registry {
    /// Remember what a container of `model_id` answered, if it is an image
    /// container that answered.
    pub(super) fn remember_image_caps(
        &self,
        class: Class,
        model_id: &str,
        caps: Option<&ImageCapabilities>,
    ) {
        let (Class::Image, Some(caps)) = (class, caps) else {
            return;
        };
        self.image_caps_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(model_id.to_string(), caps.clone());
    }

    /// What image row `model_id`'s running container answers now, read
    /// again by a caller (an upscale that found its last answer said no
    /// upscaler; sd-server rescans `upscalers/` on every read): its entry
    /// and the remembered document take it.
    pub fn refresh_image_capabilities(&self, model_id: &str, caps: ImageCapabilities) {
        if let Some(e) = self.map().get_mut(&(Class::Image, model_id.to_string())) {
            e.capabilities = Some(caps.clone());
        }
        self.remember_image_caps(Class::Image, model_id, Some(&caps));
    }

    /// Image row `model_id`'s capabilities: what its running container
    /// answered, else what the last one this process started or adopted
    /// did (module doc). `None` before any container of it answered.
    pub fn image_capabilities(&self, model_id: &str) -> Option<ImageCapabilities> {
        let running = self
            .map()
            .get(&(Class::Image, model_id.to_string()))
            .and_then(|e| e.capabilities.clone());
        running.or_else(|| {
            self.image_caps_seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(model_id)
                .cloned()
        })
    }
}
