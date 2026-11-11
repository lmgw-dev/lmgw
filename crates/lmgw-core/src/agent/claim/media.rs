//! A tool loop's GPU claim around an image or audio tool call
//! (chat-media-tools design §4.3, decision 2) — the Chat's, a
//! `/v1/responses` run's and an agent run's alike.
//!
//! Before such a call's admission the claim is let go when the route the
//! gate settled on lands on the GPU — an image row, a GPU audio row — and
//! kept for a CPU audio row or a cloud route. That is safe for the reason
//! the parent module gives: the loop's model call is over when its tools
//! run, and the next model call takes the claim again first
//! (`LoopClaim::for_call`). Parallel calls of one turn are fine: letting go
//! twice is letting go once.
//!
//! What follows is the existing scheduler and nothing else: the image row's
//! admission evicts residents with no request in flight — the chat model now
//! among them — the row stays up for its `idle_seconds`, and the regain
//! evicts it again if the chat model does not fit beside it.

use std::sync::Arc;

use super::LoopClaim;
use crate::config::{Route, Snapshot};
use crate::erased::Erased;
use crate::proxy::image_call::Hooks;
use crate::runtime::{Class, Placement};
use crate::state::SharedState;

/// The loop's hooks for its media calls.
pub(crate) struct MediaClaim {
    state: SharedState,
    claim: Arc<LoopClaim>,
}

impl MediaClaim {
    pub fn new(state: SharedState, claim: Arc<LoopClaim>) -> Self {
        Self { state, claim }
    }
}

/// Whether `route` lands on a container on the GPU.
pub(crate) fn lands_on_gpu(snap: &Snapshot, route: &Route) -> bool {
    let Some(target) = crate::vram::classify(route) else {
        return false;
    };
    match target.class {
        Class::Audio => snap
            .audio_models
            .iter()
            .find(|m| m.model_id == target.model_id)
            .is_some_and(|m| {
                crate::runtime::audio::placement(m, &snap.settings.audio) == Placement::Gpu
            }),
        Class::Aux => snap
            .aux_models
            .iter()
            .find(|m| m.model_id == target.model_id)
            .is_none_or(|m| crate::runtime::aux_models::placement(m.backend) == Placement::Gpu),
        Class::Chat | Class::Image => true,
    }
}

impl Hooks for MediaClaim {
    fn before_admit<'a>(&'a self, route: &'a Route) -> Erased<'a, ()> {
        Box::pin(async move {
            if lands_on_gpu(&self.state.snapshot(), route) {
                self.claim.let_go().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AudioModel, ImageModel};
    use serde_json::json;

    fn audio(model_id: &str, backend: Option<&str>) -> AudioModel {
        serde_json::from_value(json!({
            "id": 1, "model_id": model_id, "family": "kokoro", "path": "k", "task": "tts",
            "mode": "offline", "backend": backend, "load_options": {}, "session_options": {},
            "voice_presets": {}, "default_voice_preset": null, "enabled": true, "image": null,
            "extra_run_args": null, "warm_start": false,
        }))
        .unwrap()
    }

    fn image(model_id: &str) -> ImageModel {
        serde_json::from_value(json!({
            "id": 1, "model_id": model_id, "files": {"diffusion_model": "z.gguf"},
            "enabled": true, "image": null, "extra_run_args": null, "warm_start": false,
            "idle_seconds": 300,
        }))
        .unwrap()
    }

    #[test]
    fn only_a_route_onto_the_gpu_lets_the_claim_go() {
        let snap = Snapshot {
            image_models: vec![image("z")],
            audio_models: vec![audio("gpu-tts", None), audio("cpu-tts", Some("cpu"))],
            ..Default::default()
        };
        let route = |name: &str| snap.resolve(name).unwrap();
        assert!(lands_on_gpu(&snap, &route("image/z")));
        assert!(lands_on_gpu(&snap, &route("audio/gpu-tts")));
        assert!(!lands_on_gpu(&snap, &route("audio/cpu-tts")));
    }
}
