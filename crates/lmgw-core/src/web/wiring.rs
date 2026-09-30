//! Wiring: the whole signal path per model — HF download → GGUF on disk →
//! exposure (public name / aliases) → container — with every broken link
//! carried through as the state that names the fix.
//!
//! The composition lives in [`compose`] and is served as `GET /api/wiring`.
//! The view models are the shared DTOs, so the UI renders exactly these joins.

use lmgw_api_types as dto;

use crate::config::UpstreamKind;
use crate::state::SharedState;
use crate::store;

/// Join everything the wiring picture needs: local models × HF downloads ×
/// on-disk GGUFs × aliases × upstreams. Pure read path — no container
/// commands, no mutations.
///
/// The router-mode status fields (`router_state`/`router_running`) and the
/// `preset_in_sync` indicator left with §7: there is no shared container to
/// report a single state for, and no preset for a container to be out of sync
/// with. Per-model container state is the `runtime` list (§8).
pub(crate) async fn compose(state: &SharedState) -> dto::WiringView {
    let snap = state.snapshot();
    let models = store::list_local_models(&state.db)
        .await
        .unwrap_or_default();
    let aliases = store::list_aliases(&state.db).await.unwrap_or_default();
    let hf_rows = store::list_hf_models(&state.db).await.unwrap_or_default();
    let models_dir = snap.settings.router.models_dir.clone();
    let gguf_files = crate::hf::scan_gguf_files(&models_dir);

    let locals = models
        .iter()
        .map(|m| {
            let hf = hf_rows.iter().find(|r| r.dest_path == m.gguf_path);
            let model_aliases: Vec<String> = aliases
                .iter()
                .filter(|a| {
                    a.enabled
                        && a.upstream_model_id == m.model_id
                        && snap
                            .upstreams
                            .get(&a.upstream_id)
                            .is_some_and(|u| u.kind == UpstreamKind::LlamaServer)
                })
                .map(|a| a.alias.clone())
                .collect();
            dto::LocalChain {
                id: m.id,
                model_id: m.model_id.clone(),
                hf_repo: hf.map(|r| r.repo.clone()).unwrap_or_default(),
                hf_status: hf.map(|r| r.status.clone()).unwrap_or_default(),
                gguf_path: m.gguf_path.clone(),
                file_exists: gguf_files.contains(&m.gguf_path),
                enabled: m.enabled,
                public: m.public,
                public_name: snap.local_public_name(&m.model_id),
                aliases: model_aliases,
            }
        })
        .collect();

    let mut upstreams: Vec<dto::UpstreamChain> = snap
        .upstreams
        .values()
        .filter(|u| u.kind != UpstreamKind::LlamaServer)
        .map(|u| dto::UpstreamChain {
            id: u.id,
            name: u.name.clone(),
            protocol: u.protocol.as_str().to_string(),
            enabled: u.enabled,
            expose_all: u.expose_all,
            prefix: u.prefix().to_string(),
            aliases: aliases
                .iter()
                .filter(|a| a.enabled && a.upstream_id == u.id)
                .map(|a| a.alias.clone())
                .collect(),
        })
        .collect();
    upstreams.sort_by(|a, b| a.name.cmp(&b.name));

    let orphans = super::admin::orphan_ggufs(&gguf_files, &models)
        .into_iter()
        .map(|o| dto::OrphanGguf {
            gguf_path: o.gguf_path,
            suggested_id: o.suggested_id,
            role_guess: o.role_guess.to_string(),
        })
        .collect();

    dto::WiringView {
        locals,
        orphans,
        upstreams,
        models_dir_missing: models_dir.trim().is_empty(),
        models_dir,
        base_url: crate::net::primary_base_url(&snap.settings.bind_addr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Protocol, UpstreamKind};
    use crate::state::AppState;

    /// Fresh in-memory state whose chat models dir points at a scratch dir, so
    /// the GGUF scan has something real to walk.
    async fn state_with_models_dir() -> (SharedState, tempfile::TempDir) {
        let state = AppState::init_for_tests().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut settings = state.snapshot().settings.clone();
        settings.router.models_dir = dir.path().display().to_string();
        store::save_settings(&state.db, &settings).await.unwrap();
        state.reload_snapshot().await.unwrap();
        (state, dir)
    }

    fn touch_gguf(dir: &tempfile::TempDir, rel: &str) {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"GGUF").unwrap();
    }

    async fn add_local(state: &SharedState, model_id: &str, gguf_path: &str, public: bool) -> i64 {
        add_local_with(state, model_id, gguf_path, public, Default::default()).await
    }

    async fn add_local_with(
        state: &SharedState,
        model_id: &str,
        gguf_path: &str,
        public: bool,
        params: crate::config::LlamaParams,
    ) -> i64 {
        let id = store::insert_local_model(
            &state.db,
            &store::NewLocalModel {
                model_id: model_id.into(),
                gguf_path: gguf_path.into(),
                params,
                args: Vec::new(),
                idle_seconds: 0,
                enabled: true,
                public,
                image: None,
                extra_run_args: None,
                warm_start: false,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
                capabilities_override: None,
                ladder: Vec::new(),
            },
        )
        .await
        .unwrap();
        state.reload_snapshot().await.unwrap();
        id
    }

    #[tokio::test]
    async fn local_chain_is_wired_when_the_gguf_is_on_disk() {
        let (state, dir) = state_with_models_dir().await;
        touch_gguf(&dir, "acme/smol.gguf");
        add_local(&state, "smol", "acme/smol.gguf", true).await;

        let view = compose(&state).await;
        assert_eq!(view.locals.len(), 1);
        let c = &view.locals[0];
        assert!(c.file_exists, "scanned gguf should be found");
        assert!(c.enabled && c.public);
        assert_eq!(c.public_name, "smol");
        assert!(c.hf_repo.is_empty(), "not an HF download");
        assert!(view.orphans.is_empty(), "the file is referenced");
        assert!(!view.models_dir_missing);
        assert!(view.base_url.starts_with("http"));
    }

    /// A configured path that no longer exists is the "missing — fix path"
    /// state, and must not double as an orphan.
    #[tokio::test]
    async fn a_missing_gguf_breaks_the_chain_without_becoming_an_orphan() {
        let (state, _dir) = state_with_models_dir().await;
        add_local(&state, "gone", "acme/gone.gguf", true).await;

        let view = compose(&state).await;
        assert!(!view.locals[0].file_exists);
        assert!(view.orphans.is_empty());
    }

    #[tokio::test]
    async fn unreferenced_ggufs_are_orphans() {
        let (state, dir) = state_with_models_dir().await;
        touch_gguf(&dir, "acme/loose.gguf");

        let view = compose(&state).await;
        assert!(view.locals.is_empty());
        assert_eq!(view.orphans.len(), 1);
        assert_eq!(view.orphans[0].gguf_path, "acme/loose.gguf");
        assert!(!view.orphans[0].suggested_id.is_empty());
        assert_eq!(view.orphans[0].role_guess, "weights");
    }

    /// A projector or drafter a model loads is as referenced as its weights:
    /// listing it as "unwired" offered to wire a projector up as a chat model.
    #[tokio::test]
    async fn companions_a_model_loads_are_not_orphans() {
        let (state, dir) = state_with_models_dir().await;
        for f in [
            "acme/m-Q4_K_M.gguf",
            "acme/mmproj-BF16.gguf",
            "acme/dspark-m-Q8_0.gguf",
        ] {
            touch_gguf(&dir, f);
        }
        let params = crate::config::LlamaParams {
            mmproj_path: Some("acme/mmproj-BF16.gguf".into()),
            draft_gguf_path: Some("acme/dspark-m-Q8_0.gguf".into()),
            ..Default::default()
        };
        add_local_with(&state, "m", "acme/m-Q4_K_M.gguf", true, params).await;

        let view = compose(&state).await;
        assert!(view.orphans.is_empty(), "{:?}", view.orphans);
    }

    /// Only weights can be wired up as a model; the page files the rest
    /// under companions, so each kind has to come back as itself.
    #[tokio::test]
    async fn orphans_say_what_each_file_most_likely_is() {
        let (state, dir) = state_with_models_dir().await;
        let files = [
            ("acme/Loose-7B-Q4_K_M.gguf", "weights"),
            ("acme/mmproj-Loose-7B-f16.gguf", "mmproj"),
            ("acme/imatrix.gguf", "imatrix"),
            ("acme/dspark-Loose-7B-Q8_0.gguf", "drafter"),
            ("acme/mtp-Loose-7B-Q8_0.gguf", "drafter"),
            // Full weights that carry their own MTP layers: a drafter guess
            // here took the file's "wire up" away.
            ("acme/Loose-7B-NEO-MTP-IQ4_XS.gguf", "weights"),
        ];
        for (f, _) in files {
            touch_gguf(&dir, f);
        }

        let view = compose(&state).await;
        assert_eq!(view.orphans.len(), files.len());
        for (f, role) in files {
            let o = view.orphans.iter().find(|o| o.gguf_path == f).unwrap();
            assert_eq!(o.role_guess, role, "{f}");
        }
    }

    /// Aliases only count for a local chain when they route through a
    /// llama-server upstream; a cloud alias with the same model id must not
    /// make a private model look exposed.
    #[tokio::test]
    async fn only_llama_server_aliases_count_as_local_exposure() {
        let (state, dir) = state_with_models_dir().await;
        touch_gguf(&dir, "acme/smol.gguf");
        add_local(&state, "smol", "acme/smol.gguf", false).await;

        let cloud = store::insert_upstream(
            &state.db,
            &store::NewUpstream {
                name: "cloud".into(),
                protocol: Protocol::Openai,
                kind: UpstreamKind::Generic,
                base_url: "https://example.invalid/v1".into(),
                api_key: None,
                extra_headers: Default::default(),
                timeout_ms: 60_000,
                enabled: true,
                expose_all: false,
                expose_prefix: String::new(),
                supports_responses: false,
            },
        )
        .await
        .unwrap();
        store::insert_alias(
            &state.db,
            &store::NewAlias {
                alias: "smol-cloud".into(),
                upstream_id: cloud,
                upstream_model_id: "smol".into(),
                param_overrides: Default::default(),
                enabled: true,
                capabilities_override: None,
            },
        )
        .await
        .unwrap();
        state.reload_snapshot().await.unwrap();

        let view = compose(&state).await;
        assert!(
            view.locals[0].aliases.is_empty(),
            "cloud alias must not count as local exposure"
        );
        // …but it is still an exposure path of the remote upstream itself.
        let up = view.upstreams.iter().find(|u| u.name == "cloud").unwrap();
        assert_eq!(up.aliases, vec!["smol-cloud".to_string()]);
        assert!(!up.expose_all);
    }

    #[tokio::test]
    async fn models_dir_unset_is_reported() {
        let state = AppState::init_for_tests().await.unwrap();
        let mut settings = state.snapshot().settings.clone();
        settings.router.models_dir = String::new();
        store::save_settings(&state.db, &settings).await.unwrap();
        state.reload_snapshot().await.unwrap();

        let view = compose(&state).await;
        assert!(view.models_dir_missing);
        assert!(view.models_dir.is_empty());
    }
}
