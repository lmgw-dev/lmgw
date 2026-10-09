//! The size of an element, followed by a `ResizeObserver`.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// The content-box size of an element, kept current by a `ResizeObserver`.
///
/// A chart has to follow its own card, not the window: the card also changes
/// width when the sidebar folds to a rail, when the page body grows a
/// scrollbar, or when a container query moves the card to another column —
/// none of which is a window resize. Several observations in one frame land
/// as one update; the observer is disconnected with the owner.
pub fn use_element_size(node: NodeRef<leptos::html::Div>) -> Signal<(f64, f64)> {
    use leptos::wasm_bindgen::closure::Closure;
    use std::cell::Cell;
    use std::rc::Rc;

    let size = RwSignal::new((0.0_f64, 0.0_f64));
    Effect::new(move |_| {
        let Some(el) = node.get() else { return };
        let latest = Rc::new(Cell::new(None::<(f64, f64)>));
        let cb = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
            let Some(entry) = entries.iter().last() else {
                return;
            };
            let rect = entry
                .unchecked_into::<web_sys::ResizeObserverEntry>()
                .content_rect();
            // Only the first observation of a frame schedules the write; the
            // later ones just replace what it will write.
            if latest
                .replace(Some((rect.width(), rect.height())))
                .is_none()
            {
                let latest = latest.clone();
                request_animation_frame(move || {
                    let Some(v) = latest.take() else { return };
                    // `None` once the owner is gone: a frame can outlive it.
                    if size.try_get_untracked().is_some_and(|s| s != v) {
                        size.try_set(v);
                    }
                });
            }
        });
        let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) else {
            return;
        };
        observer.observe(&el);
        // A local arena slot: the cleanup must be `Send`, the JS handles are
        // not. Cleanups run before the owner's slots are freed.
        let held = StoredValue::new_local((observer, cb));
        on_cleanup(move || held.with_value(|(o, _)| o.disconnect()));
    });
    size.into()
}
