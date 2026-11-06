//! Renaming a conversation from its row in the list: the title turns into a
//! text box (Enter saves, Esc cancels, leaving the box saves), the way a
//! folder's name does.

use leptos::prelude::*;

/// The title box of the thread `id` being renamed, filled with `start`. Ends
/// by clearing `renaming` and, unless the title is unchanged or Esc was
/// pressed, running `on_rename` with the typed text — an empty one too, so
/// the server's refusal is what the user reads.
#[component]
pub(super) fn RenameBox(
    id: i64,
    start: String,
    renaming: RwSignal<Option<i64>>,
    on_rename: Callback<(i64, String)>,
) -> impl IntoView {
    let input: NodeRef<leptos::html::Input> = NodeRef::new();
    Effect::new(move |_| {
        if let Some(el) = input.get() {
            let _ = el.focus();
            el.select();
        }
    });
    // Enter, Esc and blur each end the edit, and only the first counts:
    // taking the box away fires a blur of its own.
    let finished = std::rc::Rc::new(std::cell::Cell::new(false));
    let unchanged = start.clone();
    // An `Rc`, not a `Callback`: the box is disposed with the edit, and the
    // blur its removal fires still calls this.
    let finish = std::rc::Rc::new(move |text: Option<String>| {
        if finished.get() {
            return;
        }
        finished.set(true);
        renaming.set(None);
        if let Some(text) = text.filter(|t| t.trim() != unchanged) {
            on_rename.run((id, text));
        }
    });
    let (on_key, on_blur) = (finish.clone(), finish);
    view! {
        <input
            class="input sm thread-rename"
            node_ref=input
            value=start
            aria-label="Conversation title"
            on:click=|ev| ev.stop_propagation()
            on:keydown=move |ev| {
                ev.stop_propagation();
                match ev.key().as_str() {
                    "Enter" => on_key(Some(event_target_value(&ev))),
                    "Escape" => on_key(None),
                    _ => {}
                }
            }
            on:blur=move |ev| on_blur(Some(event_target_value(&ev)))
        />
    }
}
