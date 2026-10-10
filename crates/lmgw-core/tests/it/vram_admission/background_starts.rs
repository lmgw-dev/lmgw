//! Background starts (§4)

use super::*;

/// Warm starts and operator starts are not requests: nothing is waiting on
/// one, so a model that does not fit is *skipped*, never allowed to evict a
/// resident that is doing work. (A real request for the same model still
/// evicts — that is the whole point of the distinction.)
#[tokio::test]
async fn a_background_start_that_does_not_fit_is_refused_instead_of_evicting() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    // 6 GiB of chat model resident, 2 GiB left, and the operator asks for a
    // 3 GiB embedder.
    assert_eq!(chat(&f.gateway).await.status(), 200);

    let err = lmgw_core::ops::container(
        &f.state,
        Some("aux"),
        Some("embed-model"),
        "start",
        false,
        None,
    )
    .await
    .expect_err("it does not fit");
    assert!(
        err.contains("embed-model") && err.contains("GPU memory"),
        "the refusal has to name the model and the reason: {err}"
    );
    assert!(
        err.contains("chat-model"),
        "…and what is holding the memory: {err}"
    );
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string()],
        "nothing was started"
    );
    assert!(
        f.stops().is_empty(),
        "and nothing was evicted for a start nobody is waiting on: {:?}",
        f.stops()
    );

    // The same start on a box where admission is not arbitrating is unchanged:
    // inactive means "behave exactly as a gateway without this feature".
    let mut s = f.state.snapshot().settings.clone();
    s.vram.enabled = false;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    crate::common::container_wire(
        lmgw_core::ops::container(
            &f.state,
            Some("aux"),
            Some("embed-model"),
            "start",
            false,
            None,
        )
        .await
        .expect("an inactive scheduler arbitrates nothing"),
    );
    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string(), "embed-model".to_string()]
    );
}

/// The boot half of the same rule: a `warm_start` model that does not fit is
/// skipped with a warning, and the models that do fit still come up. A boot
/// that evicted its way down the warm list would end with one model resident
/// and a log full of stops nobody asked for.
#[tokio::test]
async fn boot_skips_a_warm_start_that_does_not_fit() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);

    let aux = f.state.snapshot().aux_models[0].clone();
    store::update_aux_model(
        &f.state.db,
        aux.id,
        &NewAuxModel {
            model_id: aux.model_id.clone(),
            gguf_path: aux.gguf_path.clone(),
            kind: aux.kind,
            pooling: aux.pooling.clone(),
            ctx_size: aux.ctx_size,
            args: aux.args.clone(),
            idle_seconds: aux.idle_seconds,
            enabled: true,
            image: aux.image.clone(),
            extra_run_args: aux.extra_run_args.clone(),
            warm_start: true,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();

    lmgw_core::runtime::lifecycle::boot(&f.state).await;

    assert_eq!(
        f.runs(),
        vec!["chat-model".to_string()],
        "the warm start had nowhere to go and was skipped, not forced"
    );
    assert!(f.stops().is_empty(), "{:?}", f.stops());
}
