//! Aux-class internals: the small stateless model kinds — embeddings and
//! rerankers — each in its own llama-server container (§3.2).
//!
//! Exposure no longer goes through a managed `upstreams` row. Per-model
//! containers §5 deleted that row (and `ensure_aux_upstream` with it): an
//! enabled aux model resolves straight out of the `aux_models` table onto the
//! synthetic [`aux_upstream`](crate::config::Snapshot::aux_upstream), under
//! `settings.aux_router.public_prefix`. Nothing has to be provisioned for a
//! model to be routable, and nothing goes stale when the prefix changes.
//!
//! The container lifecycle this module used to own went with router mode
//! (§7): starting and stopping an aux model is [`crate::ops::container`] over
//! [`crate::runtime::registry`], the same verbs every other class gets. What
//! is left is the exposure story above, which the tests below pin.

#[cfg(test)]
mod tests {
    use crate::config::{AuxKind, AUX_UPSTREAM_ID, AUX_UPSTREAM_NAME};
    use crate::state::AppState;
    use crate::store;
    use crate::store::NewAuxModel;

    /// An enabled aux model is routable on the strength of its row alone:
    /// `embed/<id>` resolves onto the synthetic aux upstream with the prefix
    /// stripped — no alias row, no `upstreams` row to provision, and no
    /// running container. The base URL it carries is the unheld placeholder,
    /// because a local route is only forwardable through a hold (§5).
    #[tokio::test]
    async fn an_enabled_aux_model_routes_without_any_upstream_row() {
        let state = AppState::init_for_tests().await.unwrap();
        let mut settings = state.snapshot().settings.clone();
        settings.aux_router.public_prefix = "embed".into();
        store::save_settings(&state.db, &settings).await.unwrap();
        state.reload_snapshot().await.unwrap();

        store::insert_aux_model(
            &state.db,
            &NewAuxModel {
                model_id: "bge-m3".into(),
                gguf_path: "bge-m3-Q8_0.gguf".into(),
                kind: AuxKind::Embed,
                pooling: None,
                ctx_size: None,
                args: Vec::new(),
                idle_seconds: 0,
                enabled: true,
                image: None,
                extra_run_args: None,
                warm_start: false,
                hold_fallback_mode: Default::default(),
                hold_fallback: None,
            },
        )
        .await
        .unwrap();
        state.reload_snapshot().await.unwrap();

        let snap = state.snapshot();
        let route = snap.resolve("embed/bge-m3").unwrap();
        assert_eq!(route.upstream.id, AUX_UPSTREAM_ID);
        assert_eq!(route.upstream.name, AUX_UPSTREAM_NAME);
        assert_eq!(route.upstream_model, "bge-m3");
        assert!(!route.upstream.expose_all, "nothing fans out a catalog now");
        assert_eq!(route.upstream.base_url, snap.aux_upstream().base_url);
        // …and the kind gate reads the row behind that route.
        assert_eq!(
            snap.aux_model_for(&route).map(|m| m.kind),
            Some(AuxKind::Embed)
        );
        // No `upstreams` row was created for any of it.
        assert!(store::list_upstreams(&state.db).await.unwrap().is_empty());
    }
}
