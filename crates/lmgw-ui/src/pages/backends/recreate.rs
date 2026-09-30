//! "Recreate N running containers" (container-builds §6, §8): what follows
//! when an image moved under a tag while containers still run the old one —
//! after a run is made current, and after a Pull update. One per-model
//! `container apply` per affected model, in turn; never the group apply.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::{ImageUse, ImageUseKind};
use lmgw_api_types::RuntimeStatus;

use super::use_bk;
use crate::backends_api as api;
use crate::ops_state::{model_key, use_ops};

/// What [`recreate_targets`] finds: the `(class, model id)`s to recreate, and
/// the containers that are no model's.
pub type Targets = (Vec<(String, String)>, Vec<String>);

/// The running containers to recreate: `(class, model id)`, from an image's
/// users. A `running_container` use names its container; the model behind
/// it comes from the use itself when set, else from the live runtime frame.
/// The second list is what could not be matched to a model (an agent's
/// container, anything else on the machine) — said, never recreated.
pub fn recreate_targets(uses: &[ImageUse], runtime: &[RuntimeStatus]) -> Targets {
    let mut targets: Vec<(String, String)> = Vec::new();
    let mut unmatched = Vec::new();
    for u in uses
        .iter()
        .filter(|u| u.kind == ImageUseKind::RunningContainer)
    {
        let model = u.model_id.clone().or_else(|| {
            let c = u.container.as_deref()?;
            runtime
                .iter()
                .find(|r| r.container_name == c)
                .map(|r| r.model_id.clone())
        });
        match model {
            Some(m) => {
                let t = (u.class.clone(), m);
                if !targets.contains(&t) {
                    targets.push(t);
                }
            }
            None => {
                let name = u.container.clone().unwrap_or_else(|| u.class.clone());
                if !unmatched.contains(&name) {
                    unmatched.push(name);
                }
            }
        }
    }
    (targets, unmatched)
}

/// The recreate targets of `uses` against the live runtime frame.
pub fn use_targets(uses: Signal<Vec<ImageUse>>) -> Memo<Targets> {
    let bus = crate::live::use_live();
    Memo::new(move |_| {
        let rt = bus.runtime.get().unwrap_or_default();
        uses.with(|u| recreate_targets(u, &rt))
    })
}

/// The containers that are not a model's, so not offered.
#[component]
pub fn UnmatchedNote(targets: Memo<Targets>) -> impl IntoView {
    move || {
        targets.with(|(_, unmatched)| {
            (!unmatched.is_empty()).then(|| {
                view! {
                    <div class="dim mini-note">
                        {format!(
                            "Not matched to a model, so not offered for recreation: {}",
                            unmatched.join(", ")
                        )}
                    </div>
                }
            })
        })
    }
}

/// The button. Shown while `offer` holds and there is something to
/// recreate; gone once it has run. `still_on` names what the containers run
/// now, for its tooltip ("the previous image").
#[component]
pub fn RecreateButton(
    targets: Memo<Targets>,
    #[prop(into)] offer: Signal<bool>,
    still_on: &'static str,
    /// Run after the last apply (re-read what changed).
    on_done: Callback<()>,
) -> impl IntoView {
    let bk = use_bk();
    let toasts = bk.toasts;
    let ops = use_ops();
    let recreating = RwSignal::new(None::<(usize, usize)>);
    let recreated = RwSignal::new(false);
    let recreate = move |_| {
        if recreating.get_untracked().is_some() {
            return;
        }
        let list = targets.with_untracked(|(t, _)| t.clone());
        let n = list.len();
        recreating.set(Some((0, n)));
        spawn_local(async move {
            for (i, (class, model)) in list.into_iter().enumerate() {
                let key = model_key(&class, &model);
                if !ops.start(&key) {
                    toasts.warn(format!(
                        "{model}: already under another container action — skipped"
                    ));
                } else {
                    let res = api::model_apply(&class, &model).await;
                    ops.finish(&key);
                    match res
                        .map_err(|e| e.to_string())
                        .and_then(|v| api::apply_outcome(&v))
                    {
                        Ok(m) => toasts.ok(format!("{model}: {m}")),
                        Err(e) => toasts.err(format!("{model}: {e}")),
                    }
                }
                recreating.try_set(Some((i + 1, n)));
            }
            recreating.try_set(None);
            recreated.try_set(true);
            // The panel that offered it may have closed meanwhile (the
            // recreates carry on): its re-read is a no-op then.
            on_done.try_run(());
        });
    };
    move || {
        let n = targets.with(|(t, _)| t.len());
        (offer.get() && n > 0 && !recreated.get()).then(|| {
            let names = targets.with(|(t, _)| {
                t.iter()
                    .map(|(c, m)| format!("{m} ({c})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            });
            let label = move || match recreating.get() {
                Some((i, n)) => format!("Recreating {i}/{n}…"),
                None if n == 1 => "Recreate 1 running container".to_string(),
                None => format!("Recreate {n} running containers"),
            };
            view! {
                <button
                    class="btn primary"
                    disabled=move || recreating.with(Option::is_some)
                    title=format!(
                        "Still on {still_on}: {names}. Each is recreated on its own; requests in flight are refused meanwhile."
                    )
                    on:click=recreate
                >
                    {label}
                </button>
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_containers_map_to_their_models_once_each() {
        let uses = vec![
            ImageUse {
                kind: ImageUseKind::RunningContainer,
                class: "chat".into(),
                model_id: None,
                container: Some("lmgw-chat-qwen".into()),
            },
            // the same model twice (two uses of one container)
            ImageUse {
                kind: ImageUseKind::RunningContainer,
                class: "chat".into(),
                model_id: Some("qwen".into()),
                container: Some("lmgw-chat-qwen".into()),
            },
            ImageUse {
                kind: ImageUseKind::RunningContainer,
                class: "aux".into(),
                model_id: None,
                container: Some("mystery".into()),
            },
            ImageUse {
                kind: ImageUseKind::RunningContainer,
                class: "aux".into(),
                model_id: None,
                container: Some("mystery".into()),
            },
            // not a running container: never recreated from here
            ImageUse {
                kind: ImageUseKind::ClassDefault,
                class: "chat".into(),
                model_id: None,
                container: None,
            },
        ];
        let rt = vec![RuntimeStatus {
            class: "chat".into(),
            model_id: "qwen".into(),
            container_name: "lmgw-chat-qwen".into(),
            ..Default::default()
        }];
        let (t, unmatched) = recreate_targets(&uses, &rt);
        assert_eq!(t, vec![("chat".to_string(), "qwen".to_string())]);
        assert_eq!(unmatched, vec!["mystery".to_string()]);
    }
}
