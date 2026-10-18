//! The settings sections as `GET /api/settings-full` sends them: each class
//! and policy block of [`Settings`](super::Settings) as its `lmgw-api-types`
//! type. A block the gateway keeps for its own logic and a block it reports
//! share a name and a shape, so these are plain field-for-field copies, each
//! built without `..` so a field added to the reported type is a compile
//! error here, not a missing key.

use lmgw_api_types as dto;

use super::{
    AudioSettings, AuxRouterSettings, DocsSearchSettings, HoldSettings, ImageSettings,
    RouterSettings, VramSettings,
};

impl From<&RouterSettings> for dto::RouterSettings {
    fn from(r: &RouterSettings) -> Self {
        dto::RouterSettings {
            image: r.image.clone(),
            models_dir: r.models_dir.clone(),
            extra_run_args: r.extra_run_args.clone(),
            public_prefix: r.public_prefix.clone(),
            request_timeout_seconds: r.request_timeout_seconds,
        }
    }
}

impl From<&AuxRouterSettings> for dto::AuxRouterSettings {
    fn from(a: &AuxRouterSettings) -> Self {
        dto::AuxRouterSettings {
            base: (&a.base).into(),
            decision_prefix: a.decision_prefix.clone(),
        }
    }
}

impl From<&AudioSettings> for dto::AudioSettings {
    fn from(a: &AudioSettings) -> Self {
        dto::AudioSettings {
            image: a.image.clone(),
            models_dir: a.models_dir.clone(),
            backend: a.backend.clone(),
            device: a.device,
            threads: a.threads,
            lazy_load: a.lazy_load,
            busy_timeout_ms: a.busy_timeout_ms,
            idle_unload_ms: a.idle_unload_ms,
            min_free_memory_mb: a.min_free_memory_mb,
            max_request_body_mb: a.max_request_body_mb,
            voice_dir: a.voice_dir.clone(),
            extra_run_args: a.extra_run_args.clone(),
            public_prefix: a.public_prefix.clone(),
            request_timeout_seconds: a.request_timeout_seconds,
            voice_transcribe_alias: a.voice_transcribe_alias.clone(),
            catalog_revision: a.catalog_revision.as_str().to_string(),
        }
    }
}

impl From<&ImageSettings> for dto::ImageSettings {
    fn from(i: &ImageSettings) -> Self {
        dto::ImageSettings {
            image: i.image.clone(),
            models_dir: i.models_dir.clone(),
            extra_run_args: i.extra_run_args.clone(),
            public_prefix: i.public_prefix.clone(),
            request_timeout_seconds: i.request_timeout_seconds,
        }
    }
}

impl From<&VramSettings> for dto::VramSettingsDto {
    fn from(v: &VramSettings) -> Self {
        dto::VramSettingsDto {
            enabled: v.enabled,
            headroom_mb: v.headroom_mb,
            budget_mb: v.budget_mb,
            queue_timeout_seconds: v.queue_timeout_seconds,
            load_timeout_seconds: v.load_timeout_seconds,
            unload_timeout_seconds: v.unload_timeout_seconds,
            fallback_on_external: v.fallback_on_external,
        }
    }
}

impl From<&HoldSettings> for dto::HoldSettingsDto {
    fn from(h: &HoldSettings) -> Self {
        dto::HoldSettingsDto {
            active: h.active,
            fallback_alias: h.fallback_alias.clone(),
        }
    }
}

impl From<&DocsSearchSettings> for dto::DocsSearchDefaults {
    fn from(d: &DocsSearchSettings) -> Self {
        dto::DocsSearchDefaults {
            k_fts: d.k_fts,
            k_vec: d.k_vec,
            rrf_k: d.rrf_k,
            fts_weights: dto::FtsWeightsView {
                payload: d.fts_weights.payload,
                heading_path: d.fts_weights.heading_path,
                derived_title: d.fts_weights.derived_title,
                derived_summary: d.fts_weights.derived_summary,
            },
            k_rerank: d.k_rerank,
            limit: d.limit,
            budget_tokens: d.budget_tokens,
            eval_k: d.eval_k,
        }
    }
}
