//! A llama-server container VRAM admission fitted in `--fit`'s place carries
//! its placement (`lmgw.llama-fit`): boot renders the expected command from
//! it and adopts the container, with its grant, instead of removing it. A
//! row since edited to set the value itself is not adopted with the stale
//! one (finding 1 of the llama-fit review, 2026-11-11).

use super::*;
use lmgw_core::runtime::argv::{Fitted, LlamaPlacement, FITTED_LABEL};

const MIB: u64 = 1024 * 1024;

fn fitted() -> Fitted {
    Fitted {
        bytes: 2827 * MIB,
        placement: LlamaPlacement {
            ctx: Some(8192),
            n_cpu_moe: None,
            n_gpu_layers: Some(34),
        },
    }
}

/// The command the fitted start rendered: the row's, with the placement.
fn fitted_cmd(rt: &ModelRuntime, port: u16) -> Vec<String> {
    let mut render = rt
        .render_spec("lmgw", port, "/srv/models", Path::new(DATA_DIR))
        .unwrap();
    render.fitted = Some(fitted());
    render_engine_args(&render)
}

#[tokio::test]
async fn a_fitted_container_is_adopted_with_its_grant() {
    let health = container_at(200).await;
    let port = health.address().port();
    let rt = chat_runtime("m1");
    let cmd = fitted_cmd(&rt, port);
    assert!(
        cmd.windows(2).any(|w| w == ["--fit", "off"]) && cmd.contains(&"8192".to_string()),
        "{cmd:?}"
    );
    let name = container_name("lmgw", Class::Chat, "m1");
    for labelled in [true, false] {
        let mut labels = managed(Class::Chat, "m1");
        if labelled {
            labels.insert(FITTED_LABEL.to_string(), fitted().label());
        }
        let podman = Arc::new(Podman::default());
        podman.add(FakeContainer {
            name: name.clone(),
            labels,
            state: "running".into(),
            cmd: cmd.clone(),
            host_port: port,
        });
        let reg = registry(podman.clone(), vec![]);
        let report = reg.reconcile("lmgw", &[spec(&rt)]).await;
        if !labelled {
            assert!(report.adopted.is_empty(), "{:?}", report.adopted);
            assert_eq!(report.removed.len(), 1, "{:?}", report.removed);
            continue;
        }
        assert_eq!(report.adopted, vec![name.clone()], "{:?}", report.removed);
        let view = reg.list();
        let grant = view[0].grant.clone().expect("the label's grant");
        assert_eq!(grant.bytes, 2827 * MIB);
        assert_eq!(grant.llama, Some(fitted().placement));
        assert!(
            view[0].warnings.iter().any(|w| w.contains(
                "was adopted from an earlier run of lmgw, which gave it 2.8 GiB on the card and \
                 told llama-server --ctx-size 8192 --n-gpu-layers 34 --fit off"
            )),
            "{:?}",
            view[0].warnings
        );
    }
}

/// The row now sets its own context: the label's context is not rendered
/// over it, so the container's command (with the fitted one) is not the
/// row's, and it is removed rather than adopted with a stale value. Its
/// layers, still left to fitting, would have been kept.
#[tokio::test]
async fn a_row_edited_to_its_own_value_is_not_adopted_with_the_stale_one() {
    let health = container_at(200).await;
    let port = health.address().port();
    let started = chat_runtime("m1");
    let cmd = fitted_cmd(&started, port);
    let mut edited = chat_runtime("m1");
    if let Some(LlamaArgs::Chat { params, .. }) = edited.llama.as_mut() {
        params.ctx_size = Some(4096);
    }
    let name = container_name("lmgw", Class::Chat, "m1");
    let mut labels = managed(Class::Chat, "m1");
    labels.insert(FITTED_LABEL.to_string(), fitted().label());
    let podman = Arc::new(Podman::default());
    podman.add(FakeContainer {
        name,
        labels,
        state: "running".into(),
        cmd,
        host_port: port,
    });
    let reg = registry(podman.clone(), vec![]);
    let report = reg.reconcile("lmgw", &[spec(&edited)]).await;
    assert!(report.adopted.is_empty(), "{:?}", report.adopted);
    assert_eq!(report.removed.len(), 1, "{:?}", report.removed);
}
