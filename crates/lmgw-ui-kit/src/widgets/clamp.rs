//! Running text folded to its first lines (UX plan Phase 5: the agent
//! catalog cards, the agent page's head).
//!
//! "more" shows only when the text really is longer than the fold — measured
//! against the box, never guessed from a character count — and is measured
//! again whenever the box changes width, because a window that grows can make
//! a clipped description fit.

use leptos::html;
use leptos::prelude::*;

use crate::element_size::use_element_size;

/// `<div class="clamp-box">` → `.clamp` (at most `lines` lines) and a
/// "more" / "less" link beside its last line.
#[component]
pub fn ClampText(
    #[prop(into)] text: String,
    #[prop(default = 3)] lines: u8,
    /// Extra classes on the box (`agent-desc dim`).
    #[prop(optional)]
    class: &'static str,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let node: NodeRef<html::Div> = NodeRef::new();
    let size = use_element_size(node);
    let clipped = RwSignal::new(false);
    Effect::new(move |_| {
        size.track();
        // Unfolded, there is nothing to measure: the link stays as "less".
        if open.get() {
            return;
        }
        if let Some(el) = node.get() {
            let over = el.scroll_height() > el.client_height() + 1;
            if clipped.get_untracked() != over {
                clipped.set(over);
            }
        }
    });
    let root = format!("clamp-box {class}");
    view! {
        <div class=root.trim_end().to_string()>
            <div class="clamp" class:open=move || open.get() style=format!("--lines:{lines}") node_ref=node>
                {text}
            </div>
            <Show when=move || clipped.get() || open.get()>
                <button
                    type="button"
                    class="link-btn clamp-more"
                    aria-expanded=move || open.get().to_string()
                    on:click=move |_| open.update(|o| *o = !*o)
                >
                    {move || if open.get() { "less" } else { "more" }}
                </button>
            </Show>
        </div>
    }
}
