//! The image editor's Chat section (chat-media-tools design §3.4, §5.2,
//! §8.5): how the chat model writes this row's prompts, and how a running
//! picture's live preview is drawn. Human labels from the API types, the
//! same words the tool descriptions use.

use leptos::prelude::*;
use lmgw_api_types::chat_media::{PreviewMode, PromptStyle};

use crate::widgets::Select;

/// Why preview mode `m` is not available now, if it is not: the TAE mode
/// needs a TAE file, and a probed build lists what it offers.
pub(super) fn unavailable(
    m: PreviewMode,
    probed: Option<&[String]>,
    has_tae: bool,
) -> Option<String> {
    if m == PreviewMode::Tae && !has_tae {
        return Some("needs a TAE file (TAESD) in this model's files".into());
    }
    match probed {
        Some(modes) if m != PreviewMode::None && !modes.iter().any(|x| x == m.as_str()) => {
            Some("the image server this model runs on does not offer it".into())
        }
        _ => None,
    }
}

/// The preview modes the model's running container reported; none while it
/// does not run (nothing probed yet in this page's view).
fn probed_modes(model_id: &str) -> Option<Vec<String>> {
    let live = crate::live::use_live();
    live.runtime.with(|rows| {
        rows.as_ref()?
            .iter()
            .find(|r| r.class == "image" && r.model_id == model_id)
            .and_then(|r| r.image_capabilities.as_ref())
            .map(|c| c.preview_modes.clone())
            .filter(|m| !m.is_empty())
    })
}

/// The section. `prompt_style` and `preview_mode` hold the stored spelling,
/// empty for "not set" (the recipe's style, the default mode).
#[component]
pub(super) fn ImageChatFields(
    prompt_style: RwSignal<String>,
    preview_mode: RwSignal<String>,
    #[prop(into)] model_id: Signal<String>,
    #[prop(into)] has_tae: Signal<bool>,
    derived: Option<PromptStyle>,
) -> impl IntoView {
    let style_opts = Signal::derive(move || {
        let unset = match derived {
            Some(d) => format!("Derived from the recipe: {}", d.label()),
            None => "Not set".to_string(),
        };
        std::iter::once((String::new(), unset))
            .chain(PromptStyle::ALL.map(|p| (p.as_str().to_string(), p.label().to_string())))
            .collect::<Vec<_>>()
    });
    let style_note = move || {
        let picked = PromptStyle::parse(&prompt_style.get()).or(derived);
        match picked {
            Some(p) => format!("The chat model is told to {}.", p.instruction()),
            None => {
                "Not set — the chat model writes plain sentences until you pick one.".to_string()
            }
        }
    };
    let probed = Memo::new(move |_| probed_modes(&model_id.get()));
    let mode_opts = Signal::derive(move || {
        let probed = probed.get();
        let tae = has_tae.get();
        std::iter::once((
            String::new(),
            "Default: Quick sketch where offered, else Off".to_string(),
        ))
        .chain(PreviewMode::ALL.map(|m| {
            let label = match unavailable(m, probed.as_deref(), tae) {
                Some(_) => format!("{} (unavailable)", m.label()),
                None => m.label().to_string(),
            };
            (m.as_str().to_string(), label)
        }))
        .collect::<Vec<_>>()
    });
    let mode_note = move || {
        let probed = probed.get();
        match PreviewMode::parse(&preview_mode.get()) {
            Some(m) => match unavailable(m, probed.as_deref(), has_tae.get()) {
                Some(why) => format!(
                    "{} Unavailable: {why}; the picture is drawn without it.",
                    m.note()
                ),
                None => m.note().to_string(),
            },
            None => match &probed {
                Some(modes) if modes.iter().any(|m| m == "proj") => {
                    "A rough colour sketch from the sampler's state while the picture is drawn."
                        .to_string()
                }
                Some(_) => "This model's image server offers no quick sketch: progress shows \
                            elapsed time only."
                    .to_string(),
                None => "A quick sketch where the image server offers one. Which modes it \
                         offers is known once the model has started."
                    .to_string(),
            },
        }
    };
    view! {
        <section class="card edit-section" data-section="image-chat">
            <h3>"In the Chat"</h3>
            <div class="field">
                <label>"Prompt style"</label>
                <Select value=prompt_style options=style_opts/>
                <div class="dim mini-note">{style_note}</div>
            </div>
            <div class="field" style="margin-top:8px">
                <label>"Live preview"</label>
                <Select value=preview_mode options=mode_opts/>
                <div class="dim mini-note">{mode_note}</div>
            </div>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_is_unavailable_for_its_reason() {
        let probed = vec!["none".to_string(), "proj".to_string(), "vae".to_string()];
        assert!(unavailable(PreviewMode::Tae, Some(&probed), false)
            .unwrap()
            .contains("TAE file"));
        assert!(unavailable(PreviewMode::Tae, Some(&probed), true)
            .unwrap()
            .contains("does not offer"));
        assert_eq!(unavailable(PreviewMode::Proj, Some(&probed), false), None);
        // Nothing probed: only the file rule applies.
        assert_eq!(unavailable(PreviewMode::Vae, None, false), None);
        assert_eq!(unavailable(PreviewMode::None, Some(&[]), false), None);
    }
}
