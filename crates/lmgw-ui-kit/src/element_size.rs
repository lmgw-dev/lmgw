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
///
/// The first size is read when the element mounts, not left to the
/// observer: its first report lands a frame or more later (on WebKit's
/// Broadway display, hundreds of milliseconds), and until then the size is
/// `(0, 0)`. A split read that as "not measured, so wide" — it drew a panel
/// docked that the window has no room for, and a click on its rail in that
/// window docked it and stored it open, only for it to fold away the moment
/// the observer reported.
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
        let now = content_size(&el);
        if size.get_untracked() != now {
            size.set(now);
        }
        // A local arena slot: the cleanup must be `Send`, the JS handles are
        // not. Cleanups run before the owner's slots are freed.
        let held = StoredValue::new_local((observer, cb));
        on_cleanup(move || held.with_value(|(o, _)| o.disconnect()));
    });
    size.into()
}

/// The element's content box now, as a `ResizeObserver` reports it: its
/// border box less its padding and borders.
fn content_size(el: &web_sys::Element) -> (f64, f64) {
    let rect = el.get_bounding_client_rect();
    let style = web_sys::window().and_then(|w| w.get_computed_style(el).ok().flatten());
    let px = |prop: &str| {
        style
            .as_ref()
            .and_then(|s| s.get_property_value(prop).ok())
            .map_or(0.0, |v| css_px(&v))
    };
    let edges = |a: &str, b: &str| {
        px(&format!("padding-{a}"))
            + px(&format!("padding-{b}"))
            + px(&format!("border-{a}-width"))
            + px(&format!("border-{b}-width"))
    };
    (
        (rect.width() - edges("left", "right")).max(0.0),
        (rect.height() - edges("top", "bottom")).max(0.0),
    )
}

/// A computed length in px (`"12.5px"`); anything else counts as none.
fn css_px(v: &str) -> f64 {
    v.trim()
        .strip_suffix("px")
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::css_px;

    #[test]
    fn computed_lengths_read_as_px_and_anything_else_as_none() {
        assert_eq!(css_px("12.5px"), 12.5);
        assert_eq!(css_px(" 0px "), 0.0);
        assert_eq!(css_px(""), 0.0);
        assert_eq!(css_px("auto"), 0.0);
    }
}
